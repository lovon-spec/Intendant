//! Outbound Intendant Connect rendezvous client for dashboard-control signaling.
//!
//! This module intentionally implements only signaling plus opaque session-grant
//! binding. It does not authorize a browser or replace mTLS dashboard access. A
//! production Connect service must wrap this with account/passkey/device policy;
//! this client is the daemon-side transport substrate and local E2E hook.

use crate::daemon_identity::DaemonIdentity;
use crate::dashboard_control::DashboardControlRegistry;
use crate::project::ConnectConfig;
use reqwest::{Client, RequestBuilder, Url};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::{Duration, Instant};

const REGISTER_REFRESH_INTERVAL: Duration = Duration::from_secs(60);

#[derive(Debug, Serialize)]
struct RegisterRequest {
    protocol: &'static str,
    daemon_id: String,
    daemon_public_key: String,
}

#[derive(Debug, Deserialize)]
struct RegisterResponse {
    #[serde(default)]
    claimed: bool,
    #[serde(default)]
    claim_code: Option<String>,
    #[serde(default)]
    claim_url: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RendezvousEvent {
    id: String,
    kind: String,
    #[serde(default)]
    sdp: Option<String>,
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    candidate: Option<serde_json::Value>,
    #[serde(default)]
    session_grant: Option<String>,
    #[serde(default)]
    client_nonce: Option<String>,
    #[serde(default)]
    user_id: Option<String>,
    #[serde(default)]
    account_name: Option<String>,
    #[serde(default)]
    claim_id: Option<String>,
    #[serde(default)]
    challenge: Option<String>,
}

#[derive(Debug, Serialize)]
struct AnswerRequest {
    protocol: &'static str,
    daemon_id: String,
    request_id: String,
    session_id: String,
    sdp: String,
    binding: crate::dashboard_control::DashboardControlBinding,
}

#[derive(Debug, Serialize)]
struct ErrorRequest {
    daemon_id: String,
    request_id: String,
    error: String,
}

#[derive(Debug, Serialize)]
struct AckRequest {
    daemon_id: String,
    request_id: String,
    ok: bool,
}

#[derive(Debug, Serialize)]
struct ClaimProofRequest {
    protocol: &'static str,
    daemon_id: String,
    request_id: String,
    claim_id: String,
    challenge: String,
    signature: String,
}

pub fn spawn_connect_rendezvous_client(
    config: ConnectConfig,
    dashboard_control: Arc<DashboardControlRegistry>,
) -> Option<tokio::task::JoinHandle<()>> {
    if !config.enabled {
        return None;
    }
    let Some(base_url) = config
        .rendezvous_url
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    else {
        eprintln!("[connect] enabled but no rendezvous_url is configured");
        return None;
    };
    let base_url = match Url::parse(base_url) {
        Ok(url) => url,
        Err(e) => {
            eprintln!("[connect] invalid rendezvous_url {base_url:?}: {e}");
            return None;
        }
    };
    Some(tokio::spawn(async move {
        run_connect_rendezvous_client(config, base_url, dashboard_control).await;
    }))
}

async fn run_connect_rendezvous_client(
    config: ConnectConfig,
    base_url: Url,
    dashboard_control: Arc<DashboardControlRegistry>,
) {
    let identity = match DaemonIdentity::load_or_create_default() {
        Ok(identity) => identity,
        Err(e) => {
            eprintln!("[connect] daemon identity unavailable: {e}");
            return;
        }
    };
    let daemon_public_key = identity.public_key_b64u();
    let daemon_id = config
        .daemon_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| daemon_public_key.clone());
    let client = match Client::builder()
        .timeout(Duration::from_millis(
            config.poll_timeout_ms.saturating_add(10_000).max(10_000),
        ))
        .build()
    {
        Ok(client) => client,
        Err(e) => {
            eprintln!("[connect] failed to build HTTP client: {e}");
            return;
        }
    };
    let retry_delay = Duration::from_millis(config.retry_delay_ms.max(100));
    eprintln!("[connect] rendezvous client enabled for daemon {daemon_id}");

    loop {
        match register(&client, &base_url, &config, &daemon_id, &daemon_public_key).await {
            Ok(()) => {}
            Err(e) => {
                eprintln!("[connect] register failed: {e}");
                tokio::time::sleep(retry_delay).await;
                continue;
            }
        }

        let mut last_register = Instant::now();
        loop {
            match poll_next(&client, &base_url, &config, &daemon_id).await {
                Ok(Some(event)) => {
                    handle_event(
                        &client,
                        &base_url,
                        &config,
                        &daemon_id,
                        &identity,
                        &dashboard_control,
                        event,
                    )
                    .await;
                }
                Ok(None) => {
                    if last_register.elapsed() >= REGISTER_REFRESH_INTERVAL {
                        match register(&client, &base_url, &config, &daemon_id, &daemon_public_key)
                            .await
                        {
                            Ok(()) => {
                                last_register = Instant::now();
                            }
                            Err(e) => {
                                eprintln!("[connect] refresh register failed: {e}");
                                tokio::time::sleep(retry_delay).await;
                                break;
                            }
                        }
                    }
                }
                Err(e) => {
                    eprintln!("[connect] poll failed: {e}");
                    tokio::time::sleep(retry_delay).await;
                    break;
                }
            }
        }
    }
}

async fn register(
    client: &Client,
    base_url: &Url,
    config: &ConnectConfig,
    daemon_id: &str,
    daemon_public_key: &str,
) -> Result<(), String> {
    let request = RegisterRequest {
        protocol: "intendant-connect-rendezvous-v1",
        daemon_id: daemon_id.to_string(),
        daemon_public_key: daemon_public_key.to_string(),
    };
    authenticated(
        config,
        client.post(join_url(base_url, "api/daemon/register")?),
    )
    .json(&request)
    .send()
    .await
    .map_err(|e| e.to_string())?
    .error_for_status()
    .map_err(|e| e.to_string())?
    .json::<RegisterResponse>()
    .await
    .map(|response| {
        if !response.claimed {
            if let Some(url) = response.claim_url.as_deref().filter(|url| !url.is_empty()) {
                eprintln!("[connect] claim this daemon at {url}");
            } else if let Some(code) = response
                .claim_code
                .as_deref()
                .filter(|code| !code.is_empty())
            {
                eprintln!("[connect] claim this daemon with code {code}");
            }
        }
    })
    .map_err(|e| e.to_string())?;
    Ok(())
}

async fn poll_next(
    client: &Client,
    base_url: &Url,
    config: &ConnectConfig,
    daemon_id: &str,
) -> Result<Option<RendezvousEvent>, String> {
    let mut url = join_url(base_url, "api/daemon/next")?;
    url.query_pairs_mut()
        .append_pair("daemon_id", daemon_id)
        .append_pair("timeout_ms", &config.poll_timeout_ms.to_string());
    let response = authenticated(config, client.get(url))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if response.status() == reqwest::StatusCode::NO_CONTENT {
        return Ok(None);
    }
    let response = response.error_for_status().map_err(|e| e.to_string())?;
    response
        .json::<RendezvousEvent>()
        .await
        .map(Some)
        .map_err(|e| e.to_string())
}

async fn handle_event(
    client: &Client,
    base_url: &Url,
    config: &ConnectConfig,
    daemon_id: &str,
    identity: &DaemonIdentity,
    dashboard_control: &Arc<DashboardControlRegistry>,
    event: RendezvousEvent,
) {
    match event.kind.as_str() {
        "offer" => {
            let Some(sdp) = event.sdp.as_deref().filter(|s| !s.trim().is_empty()) else {
                let _ = post_error(
                    client,
                    base_url,
                    config,
                    daemon_id,
                    &event.id,
                    "missing sdp",
                )
                .await;
                return;
            };
            let session_grant = event
                .session_grant
                .as_deref()
                .map(str::trim)
                .filter(|grant| !grant.is_empty())
                .map(str::to_string);
            let client_nonce = event
                .client_nonce
                .as_deref()
                .map(str::trim)
                .filter(|nonce| !nonce.is_empty())
                .map(str::to_string);
            let grant = match connect_dashboard_grant(
                event.user_id.as_deref(),
                event.account_name.as_deref(),
            ) {
                Ok(grant) => grant,
                Err(e) => {
                    let _ = post_error(client, base_url, config, daemon_id, &event.id, &e).await;
                    return;
                }
            };
            match dashboard_control
                .answer_offer_with_grant(sdp.to_string(), session_grant, client_nonce, grant)
                .await
            {
                Ok(answer) => {
                    let body = AnswerRequest {
                        protocol: "intendant-connect-rendezvous-v1",
                        daemon_id: daemon_id.to_string(),
                        request_id: event.id,
                        session_id: answer.session_id,
                        sdp: answer.sdp,
                        binding: answer.binding,
                    };
                    if let Err(e) = authenticated(
                        config,
                        client.post(match join_url(base_url, "api/daemon/answer") {
                            Ok(url) => url,
                            Err(e) => {
                                eprintln!("[connect] answer URL failed: {e}");
                                return;
                            }
                        }),
                    )
                    .json(&body)
                    .send()
                    .await
                    .map_err(|e| e.to_string())
                    .and_then(|resp| {
                        resp.error_for_status()
                            .map(|_| ())
                            .map_err(|e| e.to_string())
                    }) {
                        eprintln!("[connect] post answer failed: {e}");
                    }
                }
                Err(e) => {
                    let _ = post_error(client, base_url, config, daemon_id, &event.id, &e).await;
                }
            }
        }
        "ice" => {
            let ok = match (event.session_id.as_deref(), event.candidate.as_ref()) {
                (Some(session_id), Some(candidate)) => dashboard_control
                    .add_ice_candidate(session_id, candidate)
                    .await
                    .unwrap_or(false),
                _ => false,
            };
            let _ = post_ack(client, base_url, config, daemon_id, &event.id, ok).await;
        }
        "close" => {
            if let Some(session_id) = event.session_id.as_deref() {
                dashboard_control.close(session_id).await;
            }
            let _ = post_ack(client, base_url, config, daemon_id, &event.id, true).await;
        }
        "claim_challenge" => {
            let Some(claim_id) = event.claim_id.as_deref().filter(|s| !s.trim().is_empty()) else {
                let _ = post_error(
                    client,
                    base_url,
                    config,
                    daemon_id,
                    &event.id,
                    "missing claim_id",
                )
                .await;
                return;
            };
            let Some(challenge) = event.challenge.as_deref().filter(|s| !s.trim().is_empty())
            else {
                let _ = post_error(
                    client,
                    base_url,
                    config,
                    daemon_id,
                    &event.id,
                    "missing claim challenge",
                )
                .await;
                return;
            };
            let payload =
                claim_signing_payload(claim_id, daemon_id, &identity.public_key_b64u(), challenge);
            let body = ClaimProofRequest {
                protocol: "intendant-connect-claim-v1",
                daemon_id: daemon_id.to_string(),
                request_id: event.id,
                claim_id: claim_id.to_string(),
                challenge: challenge.to_string(),
                signature: identity.sign_b64u(payload.as_bytes()),
            };
            if let Err(e) = authenticated(
                config,
                client.post(match join_url(base_url, "api/daemon/claim-proof") {
                    Ok(url) => url,
                    Err(e) => {
                        eprintln!("[connect] claim-proof URL failed: {e}");
                        return;
                    }
                }),
            )
            .json(&body)
            .send()
            .await
            .map_err(|e| e.to_string())
            .and_then(|resp| {
                resp.error_for_status()
                    .map(|_| ())
                    .map_err(|e| e.to_string())
            }) {
                eprintln!("[connect] post claim proof failed: {e}");
            }
        }
        other => {
            let _ = post_error(
                client,
                base_url,
                config,
                daemon_id,
                &event.id,
                &format!("unknown event kind: {other}"),
            )
            .await;
        }
    }
}

fn connect_dashboard_grant(
    user_id: Option<&str>,
    account_name: Option<&str>,
) -> Result<crate::dashboard_control::DashboardControlGrant, String> {
    let user_id = user_id.map(str::trim).filter(|value| !value.is_empty());
    let account_name = account_name
        .map(str::trim)
        .filter(|value| !value.is_empty());
    if user_id.is_none() && account_name.is_none() {
        return Err(connect_account_not_authorized_message(
            None,
            None,
            Some("the Connect offer did not include account identity"),
        ));
    }

    let cert_dir = crate::access::backend::select_backend().cert_dir();
    let path = crate::access::iam::iam_state_path(&cert_dir);
    if !path.exists() {
        return Err(connect_account_not_authorized_message(
            user_id,
            account_name,
            Some("no daemon-local IAM state exists"),
        ));
    }
    let state = crate::access::iam::load_state(&cert_dir)
        .map_err(|e| format!("local IAM state is invalid: {e}"))?;
    connect_dashboard_grant_from_state(
        state,
        user_id,
        account_name,
    )
}

fn connect_dashboard_grant_from_state(
    state: crate::access::iam::LocalIamState,
    user_id: Option<&str>,
    account_name: Option<&str>,
) -> Result<crate::dashboard_control::DashboardControlGrant, String> {
    let user_id = user_id.map(str::trim).filter(|value| !value.is_empty());
    let account_name = account_name
        .map(str::trim)
        .filter(|value| !value.is_empty());
    if user_id.is_none() && account_name.is_none() {
        return Err(connect_account_not_authorized_message(
            None,
            None,
            Some("the Connect offer did not include account identity"),
        ));
    }

    match crate::access::iam::principal_for_connect_account(
        &state,
        user_id.unwrap_or_default(),
        account_name,
        "connect-dashboard-control",
    ) {
        Some(principal) => Ok(crate::dashboard_control::DashboardControlGrant::UserClient {
            principal,
            iam_state: state,
        }),
        None => match crate::access::iam::principal_for_connect_account_any_status(
            &state,
            user_id.unwrap_or_default(),
            account_name,
            "connect-dashboard-control",
        ) {
            Some(principal) => Ok(crate::dashboard_control::DashboardControlGrant::UserClient {
                principal,
                iam_state: state,
            }),
            None => Err(connect_account_not_authorized_message(
                user_id,
                account_name,
                Some("no matching daemon-local Connect account grant exists"),
            )),
        },
    }
}

fn connect_account_not_authorized_message(
    user_id: Option<&str>,
    account_name: Option<&str>,
    detail: Option<&str>,
) -> String {
    let user_id = user_id.map(str::trim).filter(|value| !value.is_empty());
    let account_name = account_name
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let identity = match (account_name, user_id) {
        (Some(name), Some(id)) => format!("@{name} ({})", id.chars().take(12).collect::<String>()),
        (Some(name), None) => format!("@{name}"),
        (None, Some(id)) => format!("Connect account {}", id.chars().take(12).collect::<String>()),
        (None, None) => "Connect account".to_string(),
    };
    let mut message = format!(
        "{identity} is not authorized by this daemon. Open this daemon's Access page through direct mTLS/local root access and add a local IAM grant for the Connect account before using hosted Connect."
    );
    if let Some(detail) = detail.and_then(|value| {
        let value = value.trim();
        if value.is_empty() {
            None
        } else {
            Some(value)
        }
    }) {
        message.push_str(" Detail: ");
        message.push_str(detail);
        message.push('.');
    }
    message
}

fn claim_signing_payload(
    claim_id: &str,
    daemon_id: &str,
    daemon_public_key: &str,
    challenge: &str,
) -> String {
    format!(
        "intendant-connect-claim-v1\n{claim_id}\n{daemon_id}\n{daemon_public_key}\n{challenge}\n"
    )
}

async fn post_error(
    client: &Client,
    base_url: &Url,
    config: &ConnectConfig,
    daemon_id: &str,
    request_id: &str,
    error: &str,
) -> Result<(), String> {
    let body = ErrorRequest {
        daemon_id: daemon_id.to_string(),
        request_id: request_id.to_string(),
        error: error.to_string(),
    };
    authenticated(config, client.post(join_url(base_url, "api/daemon/error")?))
        .json(&body)
        .send()
        .await
        .map_err(|e| e.to_string())?
        .error_for_status()
        .map(|_| ())
        .map_err(|e| e.to_string())
}

async fn post_ack(
    client: &Client,
    base_url: &Url,
    config: &ConnectConfig,
    daemon_id: &str,
    request_id: &str,
    ok: bool,
) -> Result<(), String> {
    let body = AckRequest {
        daemon_id: daemon_id.to_string(),
        request_id: request_id.to_string(),
        ok,
    };
    authenticated(config, client.post(join_url(base_url, "api/daemon/ack")?))
        .json(&body)
        .send()
        .await
        .map_err(|e| e.to_string())?
        .error_for_status()
        .map(|_| ())
        .map_err(|e| e.to_string())
}

fn authenticated(config: &ConnectConfig, builder: RequestBuilder) -> RequestBuilder {
    match config
        .auth_token
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        Some(token) => builder.bearer_auth(token),
        None => builder,
    }
}

fn join_url(base_url: &Url, path: &str) -> Result<Url, String> {
    let mut url = base_url.clone();
    {
        let mut segments = url
            .path_segments_mut()
            .map_err(|_| "rendezvous_url cannot be a base URL".to_string())?;
        let base_segments: Vec<String> = base_url
            .path_segments()
            .map(|segments| {
                segments
                    .filter(|segment| !segment.is_empty())
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        segments.clear();
        for segment in base_segments {
            segments.push(&segment);
        }
        for segment in path.split('/').filter(|segment| !segment.is_empty()) {
            segments.push(segment);
        }
    }
    url.set_query(None);
    url.set_fragment(None);
    Ok(url)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn join_url_appends_under_base() {
        let base = Url::parse("https://connect.example/root/").unwrap();
        assert_eq!(
            join_url(&base, "api/daemon/next").unwrap().as_str(),
            "https://connect.example/root/api/daemon/next"
        );
    }

    #[test]
    fn join_url_treats_base_path_without_slash_as_directory() {
        let base = Url::parse("https://connect.example/root?ignored=true#frag").unwrap();
        assert_eq!(
            join_url(&base, "/api/daemon/next").unwrap().as_str(),
            "https://connect.example/root/api/daemon/next"
        );
    }

    #[test]
    fn connect_account_metadata_can_bind_to_scoped_local_grant() {
        let mut state = crate::access::iam::LocalIamState::default();
        state.principals.push(crate::access::iam::IamPrincipal {
            id: "principal:connect:alice".to_string(),
            kind: "connect_account".to_string(),
            label: "alice".to_string(),
            status: "active".to_string(),
            source: "local_iam_state".to_string(),
            account: Some(serde_json::json!({
                "provider": "intendant.dev",
                "account_name": "alice"
            })),
            organization: None,
            authn: vec![serde_json::json!({
                "kind": "connect_account",
                "user_id": "user-123",
                "account_name": "alice"
            })],
            notes: None,
            created_at_unix_ms: Some(100),
        });
        state.grants.push(crate::access::iam::IamGrant {
            id: "grant:connect:alice:inspect".to_string(),
            principal_id: "principal:connect:alice".to_string(),
            target_id: "local".to_string(),
            role_id: "role:scoped-human".to_string(),
            policy_id: "policy:local-user-client".to_string(),
            status: "active".to_string(),
            source: "local_iam_state".to_string(),
            reason: "test Connect account grant".to_string(),
            created_at_unix_ms: Some(101),
            revoked_at_unix_ms: None,
        });

        let grant =
            connect_dashboard_grant_from_state(state, Some("user-123"), Some("alice")).unwrap();
        let crate::dashboard_control::DashboardControlGrant::UserClient {
            principal,
            iam_state,
        } = grant
        else {
            panic!("expected scoped user-client grant");
        };
        assert_eq!(principal.kind, "connect_account");
        assert!(
            crate::access::iam::evaluate_principal_operation_with_state(
                &iam_state,
                &principal,
                crate::peer::access_policy::PeerOperation::AccessInspect,
            )
            .allowed
        );
        assert!(
            !crate::access::iam::evaluate_principal_operation_with_state(
                &iam_state,
                &principal,
                crate::peer::access_policy::PeerOperation::AccessManage,
            )
            .allowed
        );
    }

    #[test]
    fn unmatched_connect_account_metadata_requires_local_iam_grant() {
        let state = crate::access::iam::LocalIamState::default();
        let error =
            connect_dashboard_grant_from_state(state, Some("user-123"), Some("alice")).unwrap_err();
        assert!(error.contains("@alice"));
        assert!(error.contains("local IAM grant"));
        assert!(error.contains("direct mTLS"));
    }

    #[test]
    fn connect_offer_without_account_identity_is_rejected() {
        let state = crate::access::iam::LocalIamState::default();
        let error = connect_dashboard_grant_from_state(state, None, None).unwrap_err();
        assert!(error.contains("not authorized"));
        assert!(error.contains("did not include account identity"));
    }

    #[test]
    fn revoked_connect_account_binding_does_not_fall_back_to_root() {
        let mut state = crate::access::iam::LocalIamState::default();
        state.principals.push(crate::access::iam::IamPrincipal {
            id: "principal:connect:alice".to_string(),
            kind: "connect_account".to_string(),
            label: "alice".to_string(),
            status: "revoked".to_string(),
            source: "local_iam_state".to_string(),
            account: Some(serde_json::json!({
                "provider": "intendant.dev",
                "account_name": "alice"
            })),
            organization: None,
            authn: vec![serde_json::json!({
                "kind": "connect_account",
                "user_id": "user-123",
                "account_name": "alice"
            })],
            notes: None,
            created_at_unix_ms: Some(100),
        });
        state.grants.push(crate::access::iam::IamGrant {
            id: "grant:connect:alice:inspect".to_string(),
            principal_id: "principal:connect:alice".to_string(),
            target_id: "local".to_string(),
            role_id: "role:scoped-human".to_string(),
            policy_id: "policy:scoped-human".to_string(),
            status: "revoked".to_string(),
            source: "local_iam_state".to_string(),
            reason: "revoked Connect account grant".to_string(),
            created_at_unix_ms: Some(101),
            revoked_at_unix_ms: Some(102),
        });

        let grant =
            connect_dashboard_grant_from_state(state, Some("user-123"), Some("alice")).unwrap();
        let crate::dashboard_control::DashboardControlGrant::UserClient {
            principal,
            iam_state,
        } = grant
        else {
            panic!("expected inactive user-client grant, not root fallback");
        };
        assert_eq!(principal.kind, "connect_account");
        assert!(
            !crate::access::iam::evaluate_principal_operation_with_state(
                &iam_state,
                &principal,
                crate::peer::access_policy::PeerOperation::AccessInspect,
            )
            .allowed
        );
    }
}
