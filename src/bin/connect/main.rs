use axum::extract::{Path as AxumPath, Query, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode, Uri};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::Engine as _;
use bip39::Mnemonic;
use passkey_auth::{
    AuthenticationResponse, AuthenticationState, CredentialId, PasskeyCredential,
    RegistrationResponse, RegistrationState, Webauthn,
};
use rand::{rngs::OsRng, RngCore as _};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest as _, Sha256};
use std::collections::{HashMap, HashSet, VecDeque};
use std::net::SocketAddr;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::{oneshot, Mutex, Notify};
use url::{form_urlencoded, Url};
use uuid::Uuid;

const PROTOCOL: &str = "intendant-connect-rendezvous-v1";
const CLAIM_PROTOCOL: &str = "intendant-connect-claim-v1";
const COOKIE_NAME: &str = "ic_session";
const SESSION_TTL_MS: u64 = 30 * 24 * 60 * 60 * 1000;
const OFFER_TIMEOUT_MS: u64 = 30_000;
const CLAIM_TIMEOUT_MS: u64 = 60_000;
const CLAIM_CODE_TTL_MS: u64 = 10 * 60 * 1000;
const CLAIM_CODE_ENTROPY_BYTES: usize = 16;
const CLAIM_CODE_GENERATION_ATTEMPTS: usize = 32;
const ACTIVE_DASHBOARD_SESSION_TTL_MS: u64 = 24 * 60 * 60 * 1000;
const CSRF_HEADER: &str = "x-intendant-csrf";
const FLEET_TARGET_LIMIT: usize = 100;
const FLEET_TEXT_MAX: usize = 160;
const FLEET_LABEL_MAX: usize = 120;
const FLEET_URL_MAX: usize = 2048;
const FLEET_CAPABILITY_LIMIT: usize = 64;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let config = ServiceConfig::from_env_and_args()?;
    let rp_origin = Url::parse(&config.public_origin)?;
    validate_rp_id_matches_origin(&config.rp_id, &rp_origin)?;
    let webauthn = Webauthn::new(&config.rp_id, "Intendant Connect", &config.public_origin)
        .require_user_verification(true)
        .strict_base64(true);
    let store = load_store(&config.data_file)?;
    let state = Arc::new(AppState {
        config: config.clone(),
        webauthn,
        store: Mutex::new(store),
        sessions: Mutex::new(HashMap::new()),
        pending_registrations: Mutex::new(HashMap::new()),
        pending_authentications: Mutex::new(HashMap::new()),
        pending_offers: Mutex::new(HashMap::new()),
        pending_claims: Mutex::new(HashMap::new()),
        event_queues: Mutex::new(HashMap::new()),
        event_notify: Notify::new(),
        claim_codes: Mutex::new(HashMap::new()),
        rate_limits: Mutex::new(HashMap::new()),
        active_sessions: Mutex::new(HashMap::new()),
    });

    let app = Router::new()
        .route("/", get(connect_ui))
        .route("/connect", get(connect_ui))
        .route("/access", get(access_ui))
        .route("/app", get(app_html))
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        .route("/api/me", get(api_me))
        .route("/api/logout", post(api_logout))
        .route("/api/auth/register/start", post(auth_register_start))
        .route("/api/auth/register/finish", post(auth_register_finish))
        .route("/api/auth/login/start", post(auth_login_start))
        .route("/api/auth/login/finish", post(auth_login_finish))
        .route("/api/daemons", get(api_daemons))
        .route("/api/daemons/{daemon_id}/revoke", post(api_daemon_revoke))
        .route("/api/daemons/{daemon_id}/label", post(api_daemon_label))
        .route("/api/fleet/targets", get(api_fleet_targets))
        .route("/api/fleet/targets/sync", post(api_fleet_targets_sync))
        .route(
            "/api/fleet/targets/{target_id}/forget",
            post(api_fleet_target_forget),
        )
        .route("/api/claims/claim", post(api_claim_start))
        .route("/api/claims/{claim_id}", get(api_claim_status))
        .route("/api/audit", get(api_audit))
        .route("/api/status", get(api_status))
        .route("/api/daemon/register", post(daemon_register))
        .route("/api/daemon/next", get(daemon_next))
        .route("/api/daemon/answer", post(daemon_answer))
        .route("/api/daemon/error", post(daemon_error))
        .route("/api/daemon/ack", post(daemon_ack))
        .route("/api/daemon/claim-proof", post(daemon_claim_proof))
        .route("/api/browser/offer", post(browser_offer))
        .route("/api/browser/ice", post(browser_ice))
        .route("/api/browser/close", post(browser_close))
        .fallback(static_asset)
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(config.listen).await?;
    eprintln!(
        "[connect] listening on http://{} with origin {} rp_id {}",
        config.listen, config.public_origin, config.rp_id
    );
    eprintln!("[connect] state file {}", config.data_file.display());
    axum::serve(listener, app).await?;
    Ok(())
}

#[derive(Debug, Clone)]
struct ServiceConfig {
    listen: SocketAddr,
    public_origin: String,
    rp_id: String,
    static_root: PathBuf,
    data_file: PathBuf,
    daemon_token: Option<String>,
    cookie_secure: bool,
}

impl ServiceConfig {
    fn from_env_and_args() -> Result<Self, String> {
        let mut listen: SocketAddr = std::env::var("INTENDANT_CONNECT_LISTEN")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or_else(|| SocketAddr::from(([127, 0, 0, 1], 9876)));
        let mut public_origin = std::env::var("INTENDANT_CONNECT_ORIGIN")
            .ok()
            .filter(|v| !v.trim().is_empty());
        let mut rp_id = std::env::var("INTENDANT_CONNECT_RP_ID")
            .ok()
            .filter(|v| !v.trim().is_empty());
        let mut static_root = std::env::var("INTENDANT_CONNECT_STATIC_ROOT")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("static"));
        let mut data_file = std::env::var("INTENDANT_CONNECT_DATA_FILE")
            .map(PathBuf::from)
            .unwrap_or_else(|_| default_data_file());
        let mut daemon_token = std::env::var("INTENDANT_CONNECT_TOKEN")
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty());

        let mut args = std::env::args().skip(1);
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--listen" => {
                    let value = args.next().ok_or("--listen requires an address")?;
                    listen = value
                        .parse()
                        .map_err(|e| format!("invalid --listen {value:?}: {e}"))?;
                }
                "--origin" => {
                    public_origin = Some(args.next().ok_or("--origin requires a URL")?);
                }
                "--rp-id" => {
                    rp_id = Some(args.next().ok_or("--rp-id requires a domain")?);
                }
                "--static-root" => {
                    static_root =
                        PathBuf::from(args.next().ok_or("--static-root requires a path")?);
                }
                "--data-file" => {
                    data_file = PathBuf::from(args.next().ok_or("--data-file requires a path")?);
                }
                "--daemon-token" => {
                    daemon_token = Some(args.next().ok_or("--daemon-token requires a token")?);
                }
                "--help" | "-h" => {
                    print_help();
                    std::process::exit(0);
                }
                other => return Err(format!("unknown argument: {other}")),
            }
        }

        let public_origin =
            public_origin.unwrap_or_else(|| format!("http://localhost:{}", listen.port()));
        let parsed_origin = Url::parse(&public_origin)
            .map_err(|e| format!("invalid Connect origin {public_origin:?}: {e}"))?;
        let rp_id = rp_id.unwrap_or_else(|| {
            let host = parsed_origin.host_str().unwrap_or("localhost");
            if host == "intendant.dev" || host.ends_with(".intendant.dev") {
                "intendant.dev".to_string()
            } else {
                host.to_string()
            }
        });
        let cookie_secure = parsed_origin.scheme() == "https";
        Ok(Self {
            listen,
            public_origin: trim_trailing_slash(&public_origin),
            rp_id,
            static_root,
            data_file,
            daemon_token,
            cookie_secure,
        })
    }
}

fn print_help() {
    println!(
        "Usage: intendant-connect [--listen 127.0.0.1:9876] [--origin https://connect.intendant.dev] [--rp-id intendant.dev]\n\
         \n\
         Env: INTENDANT_CONNECT_LISTEN, INTENDANT_CONNECT_ORIGIN, INTENDANT_CONNECT_RP_ID,\n\
              INTENDANT_CONNECT_STATIC_ROOT, INTENDANT_CONNECT_DATA_FILE, INTENDANT_CONNECT_TOKEN"
    );
}

fn trim_trailing_slash(value: &str) -> String {
    value.trim().trim_end_matches('/').to_string()
}

fn default_data_file() -> PathBuf {
    dirs::data_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("intendant")
        .join("connect")
        .join("state.json")
}

fn validate_rp_id_matches_origin(rp_id: &str, origin: &Url) -> Result<(), String> {
    let host = origin
        .host_str()
        .ok_or_else(|| "Connect origin must include a host".to_string())?;
    if host == rp_id || host.ends_with(&format!(".{rp_id}")) {
        Ok(())
    } else {
        Err(format!(
            "rp_id {rp_id:?} is not an effective domain of origin host {host:?}"
        ))
    }
}

struct AppState {
    config: ServiceConfig,
    webauthn: Webauthn,
    store: Mutex<Store>,
    sessions: Mutex<HashMap<String, SessionRecord>>,
    pending_registrations: Mutex<HashMap<String, PendingRegistration>>,
    pending_authentications: Mutex<HashMap<String, PendingAuthentication>>,
    pending_offers: Mutex<HashMap<String, PendingOffer>>,
    pending_claims: Mutex<HashMap<String, PendingClaim>>,
    event_queues: Mutex<HashMap<String, VecDeque<RendezvousEvent>>>,
    event_notify: Notify,
    claim_codes: Mutex<HashMap<String, String>>,
    rate_limits: Mutex<HashMap<String, RateLimitBucket>>,
    active_sessions: Mutex<HashMap<String, ActiveDashboardSession>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct Store {
    #[serde(default)]
    users: Vec<UserRecord>,
    #[serde(default)]
    daemons: Vec<DaemonRecord>,
    #[serde(default)]
    fleet_targets: Vec<FleetTargetRecord>,
    #[serde(default)]
    audit: Vec<AuditEvent>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct UserRecord {
    id: Uuid,
    account_name: String,
    display_name: String,
    passkeys: Vec<PasskeyCredential>,
    created_unix_ms: u64,
    updated_unix_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct DaemonRecord {
    daemon_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    label: Option<String>,
    daemon_public_key: String,
    owner_user_id: Option<Uuid>,
    claim_code_hash: Option<String>,
    claim_code_created_unix_ms: Option<u64>,
    registered_unix_ms: u64,
    last_seen_unix_ms: u64,
    updated_unix_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct FleetTargetRecord {
    user_id: Uuid,
    id: String,
    host_id: String,
    label: String,
    #[serde(default)]
    local: bool,
    source: String,
    #[serde(default)]
    access_domain: String,
    #[serde(default)]
    access_domain_label: String,
    #[serde(default)]
    route: String,
    #[serde(default)]
    route_label: String,
    #[serde(default)]
    auth: String,
    #[serde(default)]
    auth_label: String,
    #[serde(default)]
    effective_role: String,
    #[serde(default)]
    effective_role_label: String,
    #[serde(default)]
    profile: String,
    #[serde(default)]
    url: String,
    #[serde(default)]
    ws_url: String,
    #[serde(default)]
    browser_tcp_via_url: String,
    #[serde(default)]
    origin: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    connect_daemon_id: Option<String>,
    #[serde(default)]
    capabilities: Vec<String>,
    first_seen_unix_ms: u64,
    last_seen_unix_ms: u64,
    updated_unix_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AuditEvent {
    id: String,
    unix_ms: u64,
    event: String,
    user_id: Option<Uuid>,
    daemon_id: Option<String>,
    detail: serde_json::Value,
}

#[derive(Debug, Clone)]
struct SessionRecord {
    user_id: Uuid,
    csrf_token: String,
    expires_unix_ms: u64,
}

#[derive(Debug, Clone)]
struct RateLimitBucket {
    window_start_unix_ms: u64,
    count: u32,
}

#[derive(Debug, Clone)]
struct ActiveDashboardSession {
    daemon_id: String,
    session_id: String,
    created_unix_ms: u64,
}

struct PendingRegistration {
    user_id: Uuid,
    account_name: String,
    display_name: String,
    state: RegistrationState,
    expires_unix_ms: u64,
}

struct PendingAuthentication {
    user_id: Uuid,
    state: AuthenticationState,
    expires_unix_ms: u64,
}

struct PendingOffer {
    daemon_id: String,
    user_id: Uuid,
    daemon_public_key: String,
    session_grant: String,
    response_tx: oneshot::Sender<Result<BrowserAnswerResponse, String>>,
}

#[derive(Debug, Clone)]
struct PendingClaim {
    user_id: Uuid,
    daemon_id: String,
    challenge: String,
    created_unix_ms: u64,
    status: ClaimStatus,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum ClaimStatus {
    Pending,
    Approved { daemon_id: String },
    Rejected { error: String },
}

#[derive(Debug)]
struct ApiError {
    status: StatusCode,
    message: String,
}

type ApiResult<T> = Result<T, ApiError>;

impl ApiError {
    fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }

    fn bad_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, message)
    }

    fn unauthorized(message: impl Into<String>) -> Self {
        Self::new(StatusCode::UNAUTHORIZED, message)
    }

    fn forbidden(message: impl Into<String>) -> Self {
        Self::new(StatusCode::FORBIDDEN, message)
    }

    fn not_found(message: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, message)
    }

    fn conflict(message: impl Into<String>) -> Self {
        Self::new(StatusCode::CONFLICT, message)
    }

    fn too_many_requests(message: impl Into<String>) -> Self {
        Self::new(StatusCode::TOO_MANY_REQUESTS, message)
    }

    fn internal(message: impl Into<String>) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, message)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(json!({
                "ok": false,
                "error": self.message,
            })),
        )
            .into_response()
    }
}

fn load_store(path: &Path) -> Result<Store, String> {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map_err(|e| format!("parse Connect state {}: {e}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Store::default()),
        Err(e) => Err(format!("read Connect state {}: {e}", path.display())),
    }
}

fn save_store(path: &Path, store: &Store) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("create Connect state dir {}: {e}", parent.display()))?;
    }
    let bytes = serde_json::to_vec_pretty(store).map_err(|e| format!("serialize state: {e}"))?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, bytes)
        .map_err(|e| format!("write Connect state {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path)
        .map_err(|e| format!("replace Connect state {}: {e}", path.display()))?;
    Ok(())
}

fn persist_locked(state: &AppState, store: &Store) -> ApiResult<()> {
    save_store(&state.config.data_file, store).map_err(ApiError::internal)
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn random_b64u(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    OsRng.fill_bytes(&mut buf);
    b64u(&buf)
}

fn b64u(bytes: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

fn b64u_decode(value: &str) -> Result<Vec<u8>, base64::DecodeError> {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(value)
}

fn sha256_b64u(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    b64u(&hasher.finalize())
}

fn normalize_account_name(value: &str) -> String {
    value.trim().to_ascii_lowercase()
}

fn user_view(user: &UserRecord) -> serde_json::Value {
    json!({
        "id": user.id,
        "account_name": user.account_name,
        "display_name": user.display_name,
        "passkey_count": user.passkeys.len(),
    })
}

fn daemon_view(daemon: &DaemonRecord) -> serde_json::Value {
    let now = now_unix_ms();
    json!({
        "daemon_id": daemon.daemon_id,
        "label": daemon.label,
        "daemon_public_key": daemon.daemon_public_key,
        "claimed": daemon.owner_user_id.is_some(),
        "online": now.saturating_sub(daemon.last_seen_unix_ms) < 45_000,
        "registered_unix_ms": daemon.registered_unix_ms,
        "last_seen_unix_ms": daemon.last_seen_unix_ms,
    })
}

fn daemon_fleet_target_view(config: &ServiceConfig, daemon: &DaemonRecord) -> serde_json::Value {
    let now = now_unix_ms();
    let label = daemon
        .label
        .as_deref()
        .filter(|label| !label.trim().is_empty())
        .unwrap_or(&daemon.daemon_id);
    let url = format!(
        "/app?connect=1&daemon_id={}",
        form_urlencoded::byte_serialize(daemon.daemon_id.as_bytes()).collect::<String>()
    );
    let online = now.saturating_sub(daemon.last_seen_unix_ms) < 45_000;
    json!({
        "id": daemon.daemon_id,
        "host_id": daemon.daemon_id,
        "label": label,
        "local": false,
        "source": "connect_daemon",
        "access_domain": "user_client",
        "access_domain_label": "User/client access",
        "route": "hosted_connect",
        "route_label": "Hosted Connect",
        "auth": "connect_account",
        "auth_label": "Connect account",
        "effective_role": "root",
        "effective_role_label": "Root",
        "profile": "",
        "connected": online,
        "online": online,
        "claimed_daemon": true,
        "daemon_public_key": daemon.daemon_public_key,
        "url": url,
        "ws_url": "",
        "browser_tcp_via_url": "",
        "origin": config.public_origin,
        "connect_daemon_id": daemon.daemon_id,
        "capabilities": [],
        "first_seen_unix_ms": daemon.registered_unix_ms,
        "last_seen_unix_ms": daemon.last_seen_unix_ms,
        "updated_unix_ms": daemon.updated_unix_ms,
    })
}

fn fleet_target_view(target: &FleetTargetRecord) -> serde_json::Value {
    json!({
        "id": target.id,
        "host_id": target.host_id,
        "label": target.label,
        "local": target.local,
        "source": target.source,
        "access_domain": target.access_domain,
        "access_domain_label": target.access_domain_label,
        "route": target.route,
        "route_label": target.route_label,
        "auth": target.auth,
        "auth_label": target.auth_label,
        "effective_role": target.effective_role,
        "effective_role_label": target.effective_role_label,
        "profile": target.profile,
        "connected": false,
        "online": false,
        "claimed_daemon": false,
        "daemon_public_key": "",
        "url": target.url,
        "ws_url": target.ws_url,
        "browser_tcp_via_url": target.browser_tcp_via_url,
        "origin": target.origin,
        "connect_daemon_id": target.connect_daemon_id,
        "capabilities": target.capabilities,
        "first_seen_unix_ms": target.first_seen_unix_ms,
        "last_seen_unix_ms": target.last_seen_unix_ms,
        "updated_unix_ms": target.updated_unix_ms,
    })
}

fn audit(
    store: &mut Store,
    event: &str,
    user_id: Option<Uuid>,
    daemon_id: Option<String>,
    detail: serde_json::Value,
) {
    store.audit.push(AuditEvent {
        id: Uuid::new_v4().to_string(),
        unix_ms: now_unix_ms(),
        event: event.to_string(),
        user_id,
        daemon_id,
        detail,
    });
    const MAX_AUDIT_EVENTS: usize = 2000;
    if store.audit.len() > MAX_AUDIT_EVENTS {
        let drop_count = store.audit.len() - MAX_AUDIT_EVENTS;
        store.audit.drain(0..drop_count);
    }
}

async fn healthz() -> impl IntoResponse {
    Json(json!({ "ok": true }))
}

async fn readyz(State(state): State<Arc<AppState>>) -> Response {
    let app_html = state.config.static_root.join("app.html");
    let static_ok = app_html.is_file();
    let state_parent_ok = state
        .config
        .data_file
        .parent()
        .map(|parent| parent.exists() || std::fs::create_dir_all(parent).is_ok())
        .unwrap_or(false);
    let ok = static_ok && state_parent_ok;
    let status = if ok {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (
        status,
        Json(json!({
            "ok": ok,
            "static_app": static_ok,
            "state_parent": state_parent_ok,
        })),
    )
        .into_response()
}

async fn connect_ui(State(state): State<Arc<AppState>>) -> Html<String> {
    Html(connect_ui_html(
        &state.config.public_origin,
        "Intendant Connect",
        "Passkey access",
    ))
}

async fn access_ui(State(state): State<Arc<AppState>>) -> Html<String> {
    Html(connect_ui_html(
        &state.config.public_origin,
        "Intendant Access",
        "Fleet access",
    ))
}

async fn app_html(State(state): State<Arc<AppState>>, uri: Uri) -> ApiResult<Response> {
    if !valid_connect_app_query(uri.query()) {
        return Ok(Redirect::to("/connect").into_response());
    }
    let path = state.config.static_root.join("app.html");
    serve_file(&state.config.static_root, &path)
}

fn valid_connect_app_query(query: Option<&str>) -> bool {
    let Some(query) = query else {
        return false;
    };
    let mut connect_mode = false;
    let mut daemon_id = false;
    for (key, value) in form_urlencoded::parse(query.as_bytes()) {
        match key.as_ref() {
            "connect" => connect_mode = value == "1",
            "daemon_id" => daemon_id = !value.trim().is_empty(),
            _ => {}
        }
    }
    connect_mode && daemon_id
}

async fn static_asset(State(state): State<Arc<AppState>>, uri: Uri) -> ApiResult<Response> {
    let path = safe_static_path(&state.config.static_root, uri.path())
        .ok_or_else(|| ApiError::not_found("not found"))?;
    serve_file(&state.config.static_root, &path)
}

fn safe_static_path(root: &Path, uri_path: &str) -> Option<PathBuf> {
    let trimmed = uri_path.trim_start_matches('/');
    if trimmed.is_empty() || trimmed.contains('\0') {
        return None;
    }
    let rel = Path::new(trimmed);
    if rel.components().any(|c| !matches!(c, Component::Normal(_))) {
        return None;
    }
    Some(root.join(rel))
}

fn serve_file(root: &Path, path: &Path) -> ApiResult<Response> {
    if !path.starts_with(root) || !path.is_file() {
        return Err(ApiError::not_found("not found"));
    }
    let body = std::fs::read(path).map_err(|e| ApiError::not_found(format!("not found: {e}")))?;
    let content_type = content_type_for_path(path);
    Ok((
        [(header::CONTENT_TYPE, HeaderValue::from_static(content_type))],
        body,
    )
        .into_response())
}

fn content_type_for_path(path: &Path) -> &'static str {
    match path.extension().and_then(|ext| ext.to_str()).unwrap_or("") {
        "html" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "wasm" => "application/wasm",
        "json" => "application/json; charset=utf-8",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "svg" => "image/svg+xml",
        "ico" => "image/x-icon",
        _ => "application/octet-stream",
    }
}

fn cookie_value(headers: &HeaderMap, name: &str) -> Option<String> {
    let raw = headers.get(header::COOKIE)?.to_str().ok()?;
    for part in raw.split(';') {
        let (k, v) = part.trim().split_once('=').unwrap_or((part.trim(), ""));
        if k == name && !v.is_empty() {
            return Some(v.to_string());
        }
    }
    None
}

fn session_cookie(config: &ServiceConfig, token: &str, max_age_seconds: u64) -> HeaderValue {
    let mut cookie =
        format!("{COOKIE_NAME}={token}; Max-Age={max_age_seconds}; Path=/; HttpOnly; SameSite=Lax");
    if config.cookie_secure {
        cookie.push_str("; Secure");
    }
    HeaderValue::from_str(&cookie).unwrap_or_else(|_| HeaderValue::from_static(""))
}

fn clear_session_cookie(config: &ServiceConfig) -> HeaderValue {
    let mut cookie = format!("{COOKIE_NAME}=; Max-Age=0; Path=/; HttpOnly; SameSite=Lax");
    if config.cookie_secure {
        cookie.push_str("; Secure");
    }
    HeaderValue::from_str(&cookie).unwrap_or_else(|_| HeaderValue::from_static(""))
}

async fn optional_user(state: &Arc<AppState>, headers: &HeaderMap) -> Option<UserRecord> {
    let token = cookie_value(headers, COOKIE_NAME)?;
    let now = now_unix_ms();
    let user_id = {
        let mut sessions = state.sessions.lock().await;
        let session = sessions.get(&token)?;
        if session.expires_unix_ms <= now {
            sessions.remove(&token);
            return None;
        }
        session.user_id
    };
    let store = state.store.lock().await;
    store.users.iter().find(|u| u.id == user_id).cloned()
}

async fn require_user(state: &Arc<AppState>, headers: &HeaderMap) -> ApiResult<UserRecord> {
    optional_user(state, headers)
        .await
        .ok_or_else(|| ApiError::unauthorized("sign in required"))
}

async fn create_session(state: &Arc<AppState>, user_id: Uuid) -> (String, String) {
    let token = random_b64u(32);
    let csrf_token = random_b64u(32);
    let session = SessionRecord {
        user_id,
        csrf_token: csrf_token.clone(),
        expires_unix_ms: now_unix_ms().saturating_add(SESSION_TTL_MS),
    };
    state.sessions.lock().await.insert(token.clone(), session);
    (token, csrf_token)
}

fn require_daemon_auth(state: &AppState, headers: &HeaderMap) -> ApiResult<()> {
    let Some(token) = state.config.daemon_token.as_deref() else {
        return Ok(());
    };
    let expected = format!("Bearer {token}");
    if headers
        .get(header::AUTHORIZATION)
        .and_then(|h| h.to_str().ok())
        == Some(expected.as_str())
    {
        Ok(())
    } else {
        Err(ApiError::unauthorized(
            "missing or invalid daemon bearer token",
        ))
    }
}

fn header_string(headers: &HeaderMap, name: &'static str) -> Option<String> {
    headers
        .get(name)
        .and_then(|h| h.to_str().ok())
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_string)
}

fn client_rate_key(headers: &HeaderMap, scope: &str) -> String {
    let peer = header_string(headers, "x-forwarded-for")
        .and_then(|v| v.split(',').next().map(str::trim).map(str::to_string))
        .filter(|v| !v.is_empty())
        .or_else(|| header_string(headers, "x-real-ip"))
        .unwrap_or_else(|| "unknown".to_string());
    format!("{scope}:{peer}")
}

async fn check_rate_limit(
    state: &AppState,
    headers: &HeaderMap,
    scope: &str,
    limit: u32,
    window_ms: u64,
) -> ApiResult<()> {
    let now = now_unix_ms();
    let key = client_rate_key(headers, scope);
    let mut buckets = state.rate_limits.lock().await;
    let bucket = buckets.entry(key).or_insert(RateLimitBucket {
        window_start_unix_ms: now,
        count: 0,
    });
    if now.saturating_sub(bucket.window_start_unix_ms) > window_ms {
        bucket.window_start_unix_ms = now;
        bucket.count = 0;
    }
    bucket.count = bucket.count.saturating_add(1);
    if bucket.count > limit {
        return Err(ApiError::too_many_requests("rate limit exceeded"));
    }
    Ok(())
}

fn require_same_origin(config: &ServiceConfig, headers: &HeaderMap) -> ApiResult<()> {
    let Some(origin) = header_string(headers, "origin") else {
        return Ok(());
    };
    if trim_trailing_slash(&origin) == config.public_origin {
        Ok(())
    } else {
        Err(ApiError::forbidden("request origin is not allowed"))
    }
}

async fn require_csrf(state: &Arc<AppState>, headers: &HeaderMap) -> ApiResult<()> {
    require_same_origin(&state.config, headers)?;
    let expected = header_string(headers, CSRF_HEADER)
        .ok_or_else(|| ApiError::forbidden("missing CSRF token"))?;
    let session_token = cookie_value(headers, COOKIE_NAME)
        .ok_or_else(|| ApiError::unauthorized("sign in required"))?;
    let sessions = state.sessions.lock().await;
    let session = sessions
        .get(&session_token)
        .ok_or_else(|| ApiError::unauthorized("sign in required"))?;
    if session.expires_unix_ms <= now_unix_ms() {
        return Err(ApiError::unauthorized("sign in required"));
    }
    if session.csrf_token == expected {
        Ok(())
    } else {
        Err(ApiError::forbidden("invalid CSRF token"))
    }
}

fn log_json(event: &str, detail: serde_json::Value) {
    eprintln!(
        "{}",
        json!({
            "component": "intendant-connect",
            "event": event,
            "unix_ms": now_unix_ms(),
            "detail": detail,
        })
    );
}

async fn api_me(State(state): State<Arc<AppState>>, headers: HeaderMap) -> ApiResult<Response> {
    let Some(user) = optional_user(&state, &headers).await else {
        return Ok(Json(json!({ "authenticated": false })).into_response());
    };
    let csrf_token = if let Some(token) = cookie_value(&headers, COOKIE_NAME) {
        state
            .sessions
            .lock()
            .await
            .get(&token)
            .map(|session| session.csrf_token.clone())
            .unwrap_or_default()
    } else {
        String::new()
    };
    Ok(Json(json!({
        "authenticated": true,
        "user": user_view(&user),
        "csrf_token": csrf_token,
    }))
    .into_response())
}

async fn api_logout(State(state): State<Arc<AppState>>, headers: HeaderMap) -> ApiResult<Response> {
    require_csrf(&state, &headers).await?;
    if let Some(token) = cookie_value(&headers, COOKIE_NAME) {
        state.sessions.lock().await.remove(&token);
    }
    let mut response = Json(json!({ "ok": true })).into_response();
    response
        .headers_mut()
        .insert(header::SET_COOKIE, clear_session_cookie(&state.config));
    Ok(response)
}

#[derive(Debug, Deserialize)]
struct RegisterStartRequest {
    account_name: String,
    #[serde(default)]
    display_name: String,
}

#[derive(Debug, Serialize)]
struct ChallengeStartResponse {
    ok: bool,
    flow_id: String,
    options: serde_json::Value,
}

async fn auth_register_start(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<RegisterStartRequest>,
) -> ApiResult<Json<ChallengeStartResponse>> {
    require_same_origin(&state.config, &headers)?;
    check_rate_limit(&state, &headers, "auth_register_start", 20, 60_000).await?;
    let account_name = normalize_account_name(&body.account_name);
    if account_name.is_empty() {
        return Err(ApiError::bad_request("account_name is required"));
    }
    let display_name = body.display_name.trim();
    let display_name = if display_name.is_empty() {
        account_name.clone()
    } else {
        display_name.to_string()
    };
    let (user_id, exclude_credentials) = {
        let store = state.store.lock().await;
        let existing = store.users.iter().find(|u| u.account_name == account_name);
        let user_id = existing.map(|u| u.id).unwrap_or_else(Uuid::new_v4);
        let exclude = existing
            .map(|u| {
                u.passkeys
                    .iter()
                    .map(|pk| pk.id.clone())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        (user_id, exclude)
    };
    let (options, registration) = state.webauthn.start_registration(
        user_id.as_bytes(),
        &account_name,
        &display_name,
        &exclude_credentials,
    );
    let flow_id = Uuid::new_v4().to_string();
    let pending = PendingRegistration {
        user_id,
        account_name,
        display_name,
        state: registration,
        expires_unix_ms: now_unix_ms().saturating_add(300_000),
    };
    state
        .pending_registrations
        .lock()
        .await
        .insert(flow_id.clone(), pending);
    Ok(Json(ChallengeStartResponse {
        ok: true,
        flow_id,
        options: serde_json::to_value(options).map_err(|e| ApiError::internal(e.to_string()))?,
    }))
}

#[derive(Debug, Deserialize)]
struct RegisterFinishRequest {
    flow_id: String,
    credential: RegistrationResponse,
}

async fn auth_register_finish(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<RegisterFinishRequest>,
) -> ApiResult<Response> {
    require_same_origin(&state.config, &headers)?;
    check_rate_limit(&state, &headers, "auth_register_finish", 30, 60_000).await?;
    let pending = state
        .pending_registrations
        .lock()
        .await
        .remove(body.flow_id.trim())
        .ok_or_else(|| ApiError::not_found("registration flow not found"))?;
    if pending.expires_unix_ms <= now_unix_ms() {
        return Err(ApiError::bad_request("registration flow expired"));
    }
    let passkey = state
        .webauthn
        .finish_registration(&pending.state, &body.credential)
        .map_err(|e| ApiError::bad_request(format!("finish passkey registration: {e}")))?;
    let user = {
        let mut store = state.store.lock().await;
        if store
            .users
            .iter()
            .flat_map(|u| u.passkeys.iter())
            .any(|pk| pk.id == passkey.id)
        {
            return Err(ApiError::conflict("passkey is already registered"));
        }
        let now = now_unix_ms();
        if let Some(user) = store.users.iter_mut().find(|u| u.id == pending.user_id) {
            user.display_name = pending.display_name.clone();
            user.passkeys.push(passkey);
            user.updated_unix_ms = now;
        } else {
            store.users.push(UserRecord {
                id: pending.user_id,
                account_name: pending.account_name.clone(),
                display_name: pending.display_name.clone(),
                passkeys: vec![passkey],
                created_unix_ms: now,
                updated_unix_ms: now,
            });
        }
        audit(
            &mut store,
            "passkey_registered",
            Some(pending.user_id),
            None,
            json!({ "account_name": pending.account_name }),
        );
        persist_locked(&state, &store)?;
        store
            .users
            .iter()
            .find(|u| u.id == pending.user_id)
            .cloned()
            .ok_or_else(|| ApiError::internal("created user missing"))?
    };
    let (token, csrf_token) = create_session(&state, user.id).await;
    let mut response = Json(json!({
        "ok": true,
        "user": user_view(&user),
        "csrf_token": csrf_token,
    }))
    .into_response();
    response.headers_mut().insert(
        header::SET_COOKIE,
        session_cookie(&state.config, &token, SESSION_TTL_MS / 1000),
    );
    Ok(response)
}

#[derive(Debug, Deserialize)]
struct LoginStartRequest {
    account_name: String,
}

async fn auth_login_start(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<LoginStartRequest>,
) -> ApiResult<Json<ChallengeStartResponse>> {
    require_same_origin(&state.config, &headers)?;
    check_rate_limit(&state, &headers, "auth_login_start", 30, 60_000).await?;
    let account_name = normalize_account_name(&body.account_name);
    if account_name.is_empty() {
        return Err(ApiError::bad_request("account_name is required"));
    }
    let user = {
        let store = state.store.lock().await;
        store
            .users
            .iter()
            .find(|u| u.account_name == account_name)
            .cloned()
            .ok_or_else(|| ApiError::not_found("account not found"))?
    };
    if user.passkeys.is_empty() {
        return Err(ApiError::bad_request("account has no passkeys"));
    }
    let (options, authentication) = state
        .webauthn
        .start_authentication_with_creds_for_user(user.id.as_bytes(), &user.passkeys);
    let flow_id = Uuid::new_v4().to_string();
    state.pending_authentications.lock().await.insert(
        flow_id.clone(),
        PendingAuthentication {
            user_id: user.id,
            state: authentication,
            expires_unix_ms: now_unix_ms().saturating_add(300_000),
        },
    );
    Ok(Json(ChallengeStartResponse {
        ok: true,
        flow_id,
        options: serde_json::to_value(options).map_err(|e| ApiError::internal(e.to_string()))?,
    }))
}

#[derive(Debug, Deserialize)]
struct LoginFinishRequest {
    flow_id: String,
    credential: AuthenticationResponse,
}

async fn auth_login_finish(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<LoginFinishRequest>,
) -> ApiResult<Response> {
    require_same_origin(&state.config, &headers)?;
    check_rate_limit(&state, &headers, "auth_login_finish", 60, 60_000).await?;
    let pending = state
        .pending_authentications
        .lock()
        .await
        .remove(body.flow_id.trim())
        .ok_or_else(|| ApiError::not_found("login flow not found"))?;
    if pending.expires_unix_ms <= now_unix_ms() {
        return Err(ApiError::bad_request("login flow expired"));
    }
    let user = {
        let mut store = state.store.lock().await;
        let user = store
            .users
            .iter_mut()
            .find(|u| u.id == pending.user_id)
            .ok_or_else(|| ApiError::not_found("account not found"))?;
        let asserted_id = CredentialId::from_b64url(&body.credential.id)
            .map_err(|e| ApiError::bad_request(format!("credential id: {e}")))?;
        let stored = user
            .passkeys
            .iter_mut()
            .find(|passkey| passkey.id == asserted_id)
            .ok_or_else(|| ApiError::bad_request("passkey did not match account"))?;
        let auth_result = state
            .webauthn
            .finish_authentication(&pending.state, &body.credential, stored)
            .map_err(|e| ApiError::bad_request(format!("finish passkey login: {e}")))?;
        stored.counter = auth_result.new_counter;
        user.updated_unix_ms = now_unix_ms();
        let user = user.clone();
        audit(
            &mut store,
            "passkey_login",
            Some(user.id),
            None,
            json!({ "account_name": user.account_name }),
        );
        persist_locked(&state, &store)?;
        user
    };
    let (token, csrf_token) = create_session(&state, user.id).await;
    let mut response = Json(json!({
        "ok": true,
        "user": user_view(&user),
        "csrf_token": csrf_token,
    }))
    .into_response();
    response.headers_mut().insert(
        header::SET_COOKIE,
        session_cookie(&state.config, &token, SESSION_TTL_MS / 1000),
    );
    Ok(response)
}

async fn api_daemons(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> ApiResult<Json<serde_json::Value>> {
    let user = require_user(&state, &headers).await?;
    let store = state.store.lock().await;
    let daemons = store
        .daemons
        .iter()
        .filter(|d| d.owner_user_id == Some(user.id))
        .map(daemon_view)
        .collect::<Vec<_>>();
    Ok(Json(json!({
        "ok": true,
        "daemons": daemons,
    })))
}

async fn api_fleet_targets(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> ApiResult<Json<serde_json::Value>> {
    let user = require_user(&state, &headers).await?;
    let store = state.store.lock().await;
    let targets = fleet_targets_for_user(&state.config, &store, user.id);
    Ok(Json(json!({
        "ok": true,
        "schema_version": 1,
        "targets": targets,
    })))
}

#[derive(Debug, Deserialize)]
struct FleetTargetsSyncRequest {
    #[serde(default)]
    targets: Vec<FleetTargetInput>,
}

#[derive(Debug, Deserialize)]
struct FleetTargetInput {
    #[serde(default)]
    id: String,
    #[serde(default, alias = "hostId")]
    host_id: String,
    #[serde(default)]
    label: String,
    #[serde(default)]
    local: bool,
    #[serde(default)]
    source: String,
    #[serde(default, alias = "accessDomain")]
    access_domain: String,
    #[serde(default, alias = "accessDomainLabel")]
    access_domain_label: String,
    #[serde(default)]
    route: String,
    #[serde(default)]
    route_key: String,
    #[serde(default, alias = "routeLabel")]
    route_label: String,
    #[serde(default)]
    auth: String,
    #[serde(default, alias = "authLabel")]
    auth_label: String,
    #[serde(default, alias = "effectiveRole")]
    effective_role: String,
    #[serde(default, alias = "effectiveRoleLabel")]
    effective_role_label: String,
    #[serde(default)]
    profile: String,
    #[serde(default)]
    url: String,
    #[serde(default)]
    ws_url: String,
    #[serde(default)]
    browser_tcp_via_url: String,
    #[serde(default)]
    origin: String,
    #[serde(default, alias = "connectDaemonId")]
    connect_daemon_id: String,
    #[serde(default)]
    capabilities: Vec<serde_json::Value>,
    #[serde(default, alias = "firstSeenUnixMs")]
    first_seen_unix_ms: u64,
    #[serde(default, alias = "lastSeenUnixMs")]
    last_seen_unix_ms: u64,
}

async fn api_fleet_targets_sync(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<FleetTargetsSyncRequest>,
) -> ApiResult<Json<serde_json::Value>> {
    let user = require_user(&state, &headers).await?;
    require_csrf(&state, &headers).await?;
    check_rate_limit(&state, &headers, "fleet_targets_sync", 60, 60_000).await?;
    let now = now_unix_ms();
    let mut incoming = Vec::new();
    for input in body.targets.into_iter().take(FLEET_TARGET_LIMIT) {
        if let Some(target) = normalize_fleet_target_input(user.id, input, now) {
            incoming.push(target);
        }
    }
    let mut store = state.store.lock().await;
    let owned_daemon_ids = owned_daemon_ids(&store, user.id);
    let mut by_host: HashMap<String, FleetTargetRecord> = store
        .fleet_targets
        .iter()
        .filter(|target| target.user_id == user.id)
        .map(|target| {
            let mut target = target.clone();
            canonicalize_fleet_target_for_owned_daemon(&mut target, &owned_daemon_ids);
            (target.host_id.clone(), target)
        })
        .collect();
    for mut target in incoming {
        canonicalize_fleet_target_for_owned_daemon(&mut target, &owned_daemon_ids);
        let previous = by_host.get(&target.host_id).cloned();
        let first_seen_unix_ms = previous
            .as_ref()
            .map(|record| record.first_seen_unix_ms)
            .filter(|value| *value > 0)
            .unwrap_or(target.first_seen_unix_ms);
        by_host.insert(
            target.host_id.clone(),
            FleetTargetRecord {
                first_seen_unix_ms,
                ..target
            },
        );
    }
    let mut user_targets = by_host.into_values().collect::<Vec<_>>();
    user_targets.sort_by(|a, b| {
        b.updated_unix_ms
            .cmp(&a.updated_unix_ms)
            .then_with(|| a.label.cmp(&b.label))
    });
    user_targets.truncate(FLEET_TARGET_LIMIT);
    store
        .fleet_targets
        .retain(|target| target.user_id != user.id);
    store.fleet_targets.extend(user_targets);
    persist_locked(&state, &store)?;
    let targets = fleet_targets_for_user(&state.config, &store, user.id);
    Ok(Json(json!({
        "ok": true,
        "schema_version": 1,
        "targets": targets,
    })))
}

async fn api_fleet_target_forget(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    AxumPath(target_id): AxumPath<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let user = require_user(&state, &headers).await?;
    require_csrf(&state, &headers).await?;
    check_rate_limit(&state, &headers, "fleet_target_forget", 60, 60_000).await?;
    let target_id = clean_fleet_text(&target_id, FLEET_TEXT_MAX);
    if target_id.is_empty() {
        return Err(ApiError::bad_request("target_id is required"));
    }
    let mut store = state.store.lock().await;
    let before = store.fleet_targets.len();
    store.fleet_targets.retain(|target| {
        !(target.user_id == user.id
            && (target.host_id == target_id
                || target.id == target_id
                || target.connect_daemon_id.as_deref() == Some(target_id.as_str())))
    });
    let removed = before.saturating_sub(store.fleet_targets.len());
    if removed > 0 {
        audit(
            &mut store,
            "fleet_target_forgotten",
            Some(user.id),
            Some(target_id.clone()),
            json!({ "removed": removed }),
        );
        persist_locked(&state, &store)?;
    }
    let targets = fleet_targets_for_user(&state.config, &store, user.id);
    Ok(Json(json!({
        "ok": true,
        "removed": removed,
        "schema_version": 1,
        "targets": targets,
    })))
}

fn fleet_targets_for_user(
    config: &ServiceConfig,
    store: &Store,
    user_id: Uuid,
) -> Vec<serde_json::Value> {
    let owned_daemon_ids = owned_daemon_ids(store, user_id);
    let mut by_host: HashMap<String, serde_json::Value> = HashMap::new();
    for target in store
        .fleet_targets
        .iter()
        .filter(|target| target.user_id == user_id)
    {
        let key = fleet_target_storage_key(target, &owned_daemon_ids);
        by_host.insert(key, fleet_target_view(target));
    }
    for daemon in store
        .daemons
        .iter()
        .filter(|daemon| daemon.owner_user_id == Some(user_id))
    {
        by_host.insert(
            daemon.daemon_id.clone(),
            daemon_fleet_target_view(config, daemon),
        );
    }
    let mut targets = by_host.into_values().collect::<Vec<_>>();
    targets.sort_by(|a, b| {
        let a_label = a.get("label").and_then(|v| v.as_str()).unwrap_or("");
        let b_label = b.get("label").and_then(|v| v.as_str()).unwrap_or("");
        a_label.cmp(b_label)
    });
    targets
}

fn owned_daemon_ids(store: &Store, user_id: Uuid) -> HashSet<String> {
    store
        .daemons
        .iter()
        .filter(|daemon| daemon.owner_user_id == Some(user_id))
        .map(|daemon| daemon.daemon_id.clone())
        .collect()
}

fn fleet_target_storage_key(
    target: &FleetTargetRecord,
    owned_daemon_ids: &HashSet<String>,
) -> String {
    target
        .connect_daemon_id
        .as_ref()
        .filter(|daemon_id| owned_daemon_ids.contains(*daemon_id))
        .cloned()
        .unwrap_or_else(|| target.host_id.clone())
}

fn canonicalize_fleet_target_for_owned_daemon(
    target: &mut FleetTargetRecord,
    owned_daemon_ids: &HashSet<String>,
) {
    let Some(connect_daemon_id) = target
        .connect_daemon_id
        .as_ref()
        .filter(|daemon_id| owned_daemon_ids.contains(*daemon_id))
        .cloned()
    else {
        return;
    };
    target.id = connect_daemon_id.clone();
    target.host_id = connect_daemon_id;
}

fn normalize_fleet_target_input(
    user_id: Uuid,
    input: FleetTargetInput,
    now: u64,
) -> Option<FleetTargetRecord> {
    let host_id = clean_fleet_text(
        first_non_empty(&[input.host_id.as_str(), input.id.as_str()]),
        FLEET_TEXT_MAX,
    );
    if host_id.is_empty() {
        return None;
    }
    let id = clean_fleet_text(
        first_non_empty(&[input.id.as_str(), host_id.as_str()]),
        FLEET_TEXT_MAX,
    );
    let label = clean_fleet_text(&input.label, FLEET_LABEL_MAX);
    let source = clean_fleet_token(
        first_non_empty(&[input.source.as_str(), "browser_fleet"]),
        FLEET_TEXT_MAX,
    );
    let route = clean_fleet_token(
        first_non_empty(&[input.route.as_str(), input.route_key.as_str()]),
        FLEET_TEXT_MAX,
    );
    let connect_daemon_id = clean_fleet_text(&input.connect_daemon_id, FLEET_TEXT_MAX);
    let first_seen_unix_ms = nonzero_past_or_now(input.first_seen_unix_ms, now);
    let last_seen_unix_ms = nonzero_past_or_now(input.last_seen_unix_ms, now);
    Some(FleetTargetRecord {
        user_id,
        id: if id.is_empty() { host_id.clone() } else { id },
        host_id: host_id.clone(),
        label: if label.is_empty() {
            host_id.clone()
        } else {
            label
        },
        local: input.local,
        source: if source.is_empty() {
            "browser_fleet".to_string()
        } else {
            source
        },
        access_domain: clean_fleet_token(&input.access_domain, FLEET_TEXT_MAX),
        access_domain_label: clean_fleet_text(&input.access_domain_label, FLEET_LABEL_MAX),
        route,
        route_label: clean_fleet_text(&input.route_label, FLEET_LABEL_MAX),
        auth: clean_fleet_token(&input.auth, FLEET_TEXT_MAX),
        auth_label: clean_fleet_text(&input.auth_label, FLEET_LABEL_MAX),
        effective_role: clean_fleet_token(&input.effective_role, FLEET_TEXT_MAX),
        effective_role_label: clean_fleet_text(&input.effective_role_label, FLEET_LABEL_MAX),
        profile: clean_fleet_token(&input.profile, FLEET_TEXT_MAX),
        url: clean_fleet_url(&input.url),
        ws_url: clean_fleet_url(&input.ws_url),
        browser_tcp_via_url: clean_fleet_url(&input.browser_tcp_via_url),
        origin: clean_fleet_url(&input.origin),
        connect_daemon_id: if connect_daemon_id.is_empty() {
            None
        } else {
            Some(connect_daemon_id)
        },
        capabilities: clean_fleet_capabilities(input.capabilities),
        first_seen_unix_ms,
        last_seen_unix_ms,
        updated_unix_ms: now,
    })
}

fn first_non_empty<'a>(values: &[&'a str]) -> &'a str {
    values
        .iter()
        .copied()
        .map(str::trim)
        .find(|value| !value.is_empty())
        .unwrap_or("")
}

fn clean_fleet_text(value: &str, max_chars: usize) -> String {
    value
        .trim()
        .chars()
        .filter(|ch| !ch.is_control())
        .take(max_chars)
        .collect::<String>()
}

fn clean_fleet_token(value: &str, max_chars: usize) -> String {
    clean_fleet_text(value, max_chars)
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.' | ':'))
        .collect()
}

fn clean_fleet_url(value: &str) -> String {
    let value = clean_fleet_text(value, FLEET_URL_MAX);
    if value.is_empty() {
        return String::new();
    }
    if value.starts_with('/') && !value.starts_with("//") {
        return value;
    }
    let Ok(url) = Url::parse(&value) else {
        return String::new();
    };
    match url.scheme() {
        "http" | "https" | "ws" | "wss" => value,
        _ => String::new(),
    }
}

fn clean_fleet_capabilities(values: Vec<serde_json::Value>) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for value in values.into_iter().take(FLEET_CAPABILITY_LIMIT * 2) {
        let Some(text) = value.as_str() else {
            continue;
        };
        let capability = clean_fleet_token(text, FLEET_TEXT_MAX);
        if capability.is_empty() || !seen.insert(capability.clone()) {
            continue;
        }
        out.push(capability);
        if out.len() >= FLEET_CAPABILITY_LIMIT {
            break;
        }
    }
    out
}

fn nonzero_past_or_now(value: u64, now: u64) -> u64 {
    if value == 0 || value > now {
        now
    } else {
        value
    }
}

async fn api_daemon_revoke(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    AxumPath(daemon_id): AxumPath<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let user = require_user(&state, &headers).await?;
    require_csrf(&state, &headers).await?;
    check_rate_limit(&state, &headers, "daemon_revoke", 30, 60_000).await?;
    let daemon_id = daemon_id.trim().to_string();
    ensure_owned_daemon(&state, user.id, &daemon_id).await?;
    let active_session_ids = active_dashboard_session_ids(&state, &daemon_id).await;
    let closed_sessions = active_session_ids.len();
    let mut store = state.store.lock().await;
    let daemon_index = store
        .daemons
        .iter()
        .position(|d| d.daemon_id == daemon_id)
        .ok_or_else(|| ApiError::not_found("daemon not found"))?;
    if store.daemons[daemon_index].owner_user_id != Some(user.id) {
        return Err(ApiError::forbidden("daemon belongs to a different account"));
    }
    let daemon = &mut store.daemons[daemon_index];
    daemon.owner_user_id = None;
    daemon.claim_code_hash = None;
    daemon.claim_code_created_unix_ms = None;
    daemon.updated_unix_ms = now_unix_ms();
    store.fleet_targets.retain(|target| {
        !(target.user_id == user.id
            && (target.host_id == daemon_id
                || target.id == daemon_id
                || target.connect_daemon_id.as_deref() == Some(daemon_id.as_str())))
    });
    audit(
        &mut store,
        "daemon_revoked",
        Some(user.id),
        Some(daemon_id.clone()),
        json!({ "closed_sessions": closed_sessions }),
    );
    persist_locked(&state, &store)?;
    state.claim_codes.lock().await.remove(&daemon_id);
    drop(store);
    close_active_dashboard_sessions(&state, &daemon_id, active_session_ids).await;
    log_json(
        "daemon_revoked",
        json!({ "daemon_id": daemon_id, "closed_sessions": closed_sessions }),
    );
    Ok(Json(
        json!({ "ok": true, "closed_sessions": closed_sessions }),
    ))
}

#[derive(Debug, Deserialize)]
struct DaemonLabelRequest {
    label: String,
}

async fn api_daemon_label(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    AxumPath(daemon_id): AxumPath<String>,
    Json(body): Json<DaemonLabelRequest>,
) -> ApiResult<Json<serde_json::Value>> {
    let user = require_user(&state, &headers).await?;
    require_csrf(&state, &headers).await?;
    check_rate_limit(&state, &headers, "daemon_label", 60, 60_000).await?;
    let daemon_id = daemon_id.trim().to_string();
    let label = body.label.trim();
    if label.len() > 80 {
        return Err(ApiError::bad_request(
            "label must be 80 characters or shorter",
        ));
    }
    let mut store = state.store.lock().await;
    let daemon_index = store
        .daemons
        .iter()
        .position(|d| d.daemon_id == daemon_id)
        .ok_or_else(|| ApiError::not_found("daemon not found"))?;
    if store.daemons[daemon_index].owner_user_id != Some(user.id) {
        return Err(ApiError::forbidden("daemon belongs to a different account"));
    }
    let daemon = &mut store.daemons[daemon_index];
    daemon.label = if label.is_empty() {
        None
    } else {
        Some(label.to_string())
    };
    daemon.updated_unix_ms = now_unix_ms();
    let view = daemon_view(daemon);
    let target_label = if label.is_empty() {
        daemon_id.as_str()
    } else {
        label
    };
    let now = now_unix_ms();
    for target in store.fleet_targets.iter_mut().filter(|target| {
        target.user_id == user.id
            && (target.host_id == daemon_id
                || target.id == daemon_id
                || target.connect_daemon_id.as_deref() == Some(daemon_id.as_str()))
    }) {
        target.label = target_label.to_string();
        target.updated_unix_ms = now;
    }
    audit(
        &mut store,
        "daemon_label_updated",
        Some(user.id),
        Some(daemon_id.clone()),
        json!({ "label": label }),
    );
    persist_locked(&state, &store)?;
    Ok(Json(json!({ "ok": true, "daemon": view })))
}

#[derive(Debug, Deserialize)]
struct ClaimStartRequest {
    claim_code: String,
}

async fn api_claim_start(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<ClaimStartRequest>,
) -> ApiResult<Json<serde_json::Value>> {
    let user = require_user(&state, &headers).await?;
    require_csrf(&state, &headers).await?;
    check_rate_limit(&state, &headers, "claim_start", 10, 60_000).await?;
    let code = normalize_claim_code(&body.claim_code);
    if code.is_empty() {
        return Err(ApiError::bad_request("claim_code is required"));
    }
    let code_hashes = claim_code_hash_candidates(&body.claim_code);
    let now = now_unix_ms();
    let daemon = {
        let store = state.store.lock().await;
        store
            .daemons
            .iter()
            .find(|d| {
                d.owner_user_id.is_none()
                    && d.claim_code_hash
                        .as_deref()
                        .is_some_and(|hash| code_hashes.iter().any(|candidate| candidate == hash))
                    && d.claim_code_created_unix_ms
                        .is_some_and(|created| now.saturating_sub(created) <= CLAIM_CODE_TTL_MS)
            })
            .cloned()
            .ok_or_else(|| ApiError::not_found("claim code not found"))?
    };
    let claim_id = Uuid::new_v4().to_string();
    let challenge = random_b64u(32);
    state.pending_claims.lock().await.insert(
        claim_id.clone(),
        PendingClaim {
            user_id: user.id,
            daemon_id: daemon.daemon_id.clone(),
            challenge: challenge.clone(),
            created_unix_ms: now_unix_ms(),
            status: ClaimStatus::Pending,
        },
    );
    enqueue_event(
        &state,
        &daemon.daemon_id,
        RendezvousEvent {
            id: Uuid::new_v4().to_string(),
            kind: "claim_challenge".to_string(),
            claim_id: Some(claim_id.clone()),
            challenge: Some(challenge),
            ..RendezvousEvent::default()
        },
    )
    .await;
    {
        let mut store = state.store.lock().await;
        audit(
            &mut store,
            "daemon_claim_started",
            Some(user.id),
            Some(daemon.daemon_id.clone()),
            json!({ "claim_id": claim_id }),
        );
        persist_locked(&state, &store)?;
    }
    Ok(Json(json!({
        "ok": true,
        "claim_id": claim_id,
        "daemon_id": daemon.daemon_id,
    })))
}

async fn api_claim_status(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    AxumPath(claim_id): AxumPath<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let user = require_user(&state, &headers).await?;
    let mut claims = state.pending_claims.lock().await;
    let claim = claims
        .get_mut(claim_id.trim())
        .ok_or_else(|| ApiError::not_found("claim not found"))?;
    if claim.user_id != user.id {
        return Err(ApiError::forbidden("claim belongs to a different account"));
    }
    if matches!(claim.status, ClaimStatus::Pending)
        && now_unix_ms().saturating_sub(claim.created_unix_ms) > CLAIM_TIMEOUT_MS
    {
        claim.status = ClaimStatus::Rejected {
            error: "claim timed out".to_string(),
        };
    }
    Ok(Json(json!({
        "ok": true,
        "claim_id": claim_id,
        "daemon_id": claim.daemon_id,
        "result": claim.status,
    })))
}

async fn api_audit(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> ApiResult<Json<serde_json::Value>> {
    let user = require_user(&state, &headers).await?;
    let store = state.store.lock().await;
    let events = store
        .audit
        .iter()
        .filter(|event| event.user_id == Some(user.id))
        .rev()
        .take(100)
        .cloned()
        .collect::<Vec<_>>();
    Ok(Json(json!({
        "ok": true,
        "events": events,
    })))
}

#[derive(Debug, Deserialize)]
struct StatusQuery {
    #[serde(default)]
    daemon_id: String,
}

async fn api_status(
    State(state): State<Arc<AppState>>,
    Query(query): Query<StatusQuery>,
) -> Json<serde_json::Value> {
    let daemon_id = query.daemon_id.trim();
    let (daemon, queued, active_sessions) = {
        let store = state.store.lock().await;
        let daemon = store
            .daemons
            .iter()
            .find(|d| d.daemon_id == daemon_id)
            .cloned();
        let queued = state
            .event_queues
            .lock()
            .await
            .get(daemon_id)
            .map(|q| q.len())
            .unwrap_or(0);
        let active_sessions = state
            .active_sessions
            .lock()
            .await
            .values()
            .filter(|session| session.daemon_id == daemon_id)
            .count();
        (daemon, queued, active_sessions)
    };
    let now = now_unix_ms();
    let claim_code_expires_unix_ms = daemon
        .as_ref()
        .and_then(|d| d.claim_code_created_unix_ms)
        .map(|created| created.saturating_add(CLAIM_CODE_TTL_MS))
        .filter(|expires| *expires > now);
    Json(json!({
        "ok": true,
        "daemon_id": daemon_id,
        "registered": daemon.is_some(),
        "claimed": daemon.as_ref().and_then(|d| d.owner_user_id).is_some(),
        "label": daemon.as_ref().and_then(|d| d.label.as_deref()).unwrap_or(""),
        "daemon_public_key": daemon.as_ref().map(|d| d.daemon_public_key.as_str()).unwrap_or(""),
        "last_seen_unix_ms": daemon.as_ref().map(|d| d.last_seen_unix_ms).unwrap_or(0),
        "claim_code_expires_unix_ms": claim_code_expires_unix_ms,
        "queued": queued,
        "active_sessions": active_sessions,
        "daemon_auth_required": state.config.daemon_token.is_some(),
    }))
}

#[derive(Debug, Deserialize)]
struct DaemonRegisterRequest {
    protocol: String,
    daemon_id: String,
    daemon_public_key: String,
}

async fn daemon_register(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<DaemonRegisterRequest>,
) -> ApiResult<Json<serde_json::Value>> {
    require_daemon_auth(&state, &headers)?;
    check_rate_limit(&state, &headers, "daemon_register", 120, 60_000).await?;
    if body.protocol != PROTOCOL {
        return Err(ApiError::bad_request("unsupported protocol"));
    }
    let daemon_id = body.daemon_id.trim().to_string();
    let daemon_public_key = body.daemon_public_key.trim().to_string();
    if daemon_id.is_empty() || daemon_public_key.is_empty() {
        return Err(ApiError::bad_request(
            "daemon_id and daemon_public_key are required",
        ));
    }
    let mut claim_code = None;
    let claimed = {
        let mut claim_codes = state.claim_codes.lock().await;
        let mut store = state.store.lock().await;
        let now = now_unix_ms();
        let active_claim_hashes = active_claim_code_hashes(&store, &daemon_id, now);
        let claimed_now = if let Some(existing) =
            store.daemons.iter_mut().find(|d| d.daemon_id == daemon_id)
        {
            if existing.owner_user_id.is_some() && existing.daemon_public_key != daemon_public_key {
                return Err(ApiError::conflict(
                    "claimed daemon_id is already bound to a different daemon key",
                ));
            }
            existing.daemon_public_key = daemon_public_key.clone();
            existing.last_seen_unix_ms = now;
            existing.updated_unix_ms = now;
            if existing.owner_user_id.is_none() {
                claim_code = Some(ensure_claim_code(
                    &mut claim_codes,
                    existing,
                    &active_claim_hashes,
                )?);
            }
            existing.owner_user_id.is_some()
        } else {
            let mut record = DaemonRecord {
                daemon_id: daemon_id.clone(),
                label: None,
                daemon_public_key: daemon_public_key.clone(),
                owner_user_id: None,
                claim_code_hash: None,
                claim_code_created_unix_ms: None,
                registered_unix_ms: now,
                last_seen_unix_ms: now,
                updated_unix_ms: now,
            };
            claim_code = Some(ensure_claim_code(
                &mut claim_codes,
                &mut record,
                &active_claim_hashes,
            )?);
            store.daemons.push(record);
            false
        };
        persist_locked(&state, &store)?;
        claimed_now
    };
    let claim_url = claim_code
        .as_ref()
        .map(|code| format!("{}/connect?claim_code={code}", state.config.public_origin));
    if let Some(url) = claim_url.as_deref() {
        log_json(
            "daemon_awaiting_claim",
            json!({ "daemon_id": daemon_id, "claim_url": url }),
        );
    }
    Ok(Json(json!({
        "ok": true,
        "claimed": claimed,
        "claim_code": claim_code,
        "claim_url": claim_url,
        "daemon_public_key": daemon_public_key,
    })))
}

fn ensure_claim_code(
    claim_codes: &mut HashMap<String, String>,
    daemon: &mut DaemonRecord,
    active_claim_hashes: &HashSet<String>,
) -> ApiResult<String> {
    let now = now_unix_ms();
    let existing_is_fresh = daemon
        .claim_code_created_unix_ms
        .is_some_and(|created| now.saturating_sub(created) <= CLAIM_CODE_TTL_MS);
    let existing_hash_is_unique = daemon
        .claim_code_hash
        .as_deref()
        .is_some_and(|hash| !active_claim_hashes.contains(hash));
    if existing_is_fresh && existing_hash_is_unique {
        if let Some(code) = claim_codes.get(&daemon.daemon_id).cloned() {
            return Ok(code);
        }
    }
    if !existing_is_fresh {
        claim_codes.remove(&daemon.daemon_id);
    }
    for _ in 0..CLAIM_CODE_GENERATION_ATTEMPTS {
        let code = generate_claim_code()?;
        let code_hash = claim_code_hash(&code);
        if active_claim_hashes.contains(&code_hash) {
            continue;
        }
        daemon.claim_code_hash = Some(code_hash);
        daemon.claim_code_created_unix_ms = Some(now);
        claim_codes.insert(daemon.daemon_id.clone(), code.clone());
        return Ok(code);
    }
    Err(ApiError::internal("failed to generate a unique claim code"))
}

fn generate_claim_code() -> ApiResult<String> {
    let mut entropy = [0u8; CLAIM_CODE_ENTROPY_BYTES];
    OsRng.fill_bytes(&mut entropy);
    let mnemonic = Mnemonic::from_entropy(&entropy)
        .map_err(|e| ApiError::internal(format!("generate claim mnemonic: {e}")))?;
    Ok(mnemonic.to_string().replace(' ', "-"))
}

fn active_claim_code_hashes(store: &Store, except_daemon_id: &str, now: u64) -> HashSet<String> {
    store
        .daemons
        .iter()
        .filter(|daemon| daemon.daemon_id != except_daemon_id)
        .filter(|daemon| daemon.owner_user_id.is_none())
        .filter(|daemon| {
            daemon
                .claim_code_created_unix_ms
                .is_some_and(|created| now.saturating_sub(created) <= CLAIM_CODE_TTL_MS)
        })
        .filter_map(|daemon| daemon.claim_code_hash.clone())
        .collect()
}

fn claim_code_hash(code: &str) -> String {
    sha256_b64u(normalize_claim_code(code).as_bytes())
}

fn claim_code_hash_candidates(input: &str) -> Vec<String> {
    let mut hashes = Vec::with_capacity(2);
    let normalized = normalize_claim_code(input);
    if !normalized.is_empty() {
        hashes.push(sha256_b64u(normalized.as_bytes()));
    }
    let legacy = input.trim().replace(' ', "").to_ascii_uppercase();
    if !legacy.is_empty() && legacy != normalized {
        let hash = sha256_b64u(legacy.as_bytes());
        if !hashes.iter().any(|existing| existing == &hash) {
            hashes.push(hash);
        }
    }
    hashes
}

fn normalize_claim_code(input: &str) -> String {
    let mut parts = Vec::new();
    let mut current = String::new();
    for ch in input.chars() {
        if ch.is_ascii_alphanumeric() {
            current.push(ch.to_ascii_lowercase());
        } else if !current.is_empty() {
            parts.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        parts.push(current);
    }
    parts.join("-")
}

#[derive(Debug, Deserialize)]
struct DaemonNextQuery {
    daemon_id: String,
    #[serde(default)]
    timeout_ms: Option<u64>,
}

async fn daemon_next(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(query): Query<DaemonNextQuery>,
) -> ApiResult<Response> {
    require_daemon_auth(&state, &headers)?;
    check_rate_limit(&state, &headers, "daemon_next", 240, 60_000).await?;
    let daemon_id = query.daemon_id.trim().to_string();
    if daemon_id.is_empty() {
        return Err(ApiError::bad_request("daemon_id is required"));
    }
    touch_daemon(&state, &daemon_id).await?;
    let timeout = Duration::from_millis(query.timeout_ms.unwrap_or(15_000).min(30_000));
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if let Some(event) = pop_event(&state, &daemon_id).await {
            return Ok(Json(event).into_response());
        }
        let now = tokio::time::Instant::now();
        if now >= deadline {
            return Ok(StatusCode::NO_CONTENT.into_response());
        }
        let remaining = deadline.saturating_duration_since(now);
        if tokio::time::timeout(remaining, state.event_notify.notified())
            .await
            .is_err()
        {
            return Ok(StatusCode::NO_CONTENT.into_response());
        }
    }
}

async fn touch_daemon(state: &AppState, daemon_id: &str) -> ApiResult<()> {
    let mut store = state.store.lock().await;
    if let Some(daemon) = store.daemons.iter_mut().find(|d| d.daemon_id == daemon_id) {
        daemon.last_seen_unix_ms = now_unix_ms();
        daemon.updated_unix_ms = daemon.last_seen_unix_ms;
        persist_locked(state, &store)?;
        Ok(())
    } else {
        Err(ApiError::not_found("daemon is not registered"))
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct RendezvousEvent {
    id: String,
    kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    sdp: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    candidate: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    session_grant: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    client_nonce: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    user_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    account_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    claim_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    challenge: Option<String>,
}

async fn enqueue_event(state: &AppState, daemon_id: &str, event: RendezvousEvent) {
    let mut queues = state.event_queues.lock().await;
    queues
        .entry(daemon_id.to_string())
        .or_default()
        .push_back(event);
    drop(queues);
    state.event_notify.notify_waiters();
}

async fn pop_event(state: &AppState, daemon_id: &str) -> Option<RendezvousEvent> {
    let mut queues = state.event_queues.lock().await;
    let queue = queues.get_mut(daemon_id)?;
    let event = queue.pop_front();
    if queue.is_empty() {
        queues.remove(daemon_id);
    }
    event
}

async fn record_active_dashboard_session(state: &AppState, daemon_id: &str, session_id: &str) {
    let now = now_unix_ms();
    let mut sessions = state.active_sessions.lock().await;
    sessions.retain(|_, session| {
        now.saturating_sub(session.created_unix_ms) <= ACTIVE_DASHBOARD_SESSION_TTL_MS
    });
    sessions.insert(
        session_id.to_string(),
        ActiveDashboardSession {
            daemon_id: daemon_id.to_string(),
            session_id: session_id.to_string(),
            created_unix_ms: now,
        },
    );
}

async fn active_dashboard_session_ids(state: &AppState, daemon_id: &str) -> Vec<String> {
    let now = now_unix_ms();
    let mut active = state.active_sessions.lock().await;
    active.retain(|_, session| {
        now.saturating_sub(session.created_unix_ms) <= ACTIVE_DASHBOARD_SESSION_TTL_MS
    });
    active
        .values()
        .filter(|session| session.daemon_id == daemon_id)
        .map(|session| session.session_id.clone())
        .collect()
}

async fn close_active_dashboard_sessions(
    state: &AppState,
    daemon_id: &str,
    session_ids: Vec<String>,
) -> usize {
    let sessions = {
        let mut active = state.active_sessions.lock().await;
        let mut sessions = Vec::new();
        for session_id in session_ids {
            let belongs_to_daemon = active
                .get(&session_id)
                .is_some_and(|session| session.daemon_id == daemon_id);
            if belongs_to_daemon {
                active.remove(&session_id);
                sessions.push(session_id);
            }
        }
        sessions
    };
    let closed = sessions.len();
    for session_id in sessions {
        enqueue_event(
            state,
            daemon_id,
            RendezvousEvent {
                id: Uuid::new_v4().to_string(),
                kind: "close".to_string(),
                session_id: Some(session_id),
                ..RendezvousEvent::default()
            },
        )
        .await;
    }
    closed
}

#[derive(Debug, Deserialize)]
struct DaemonAnswerRequest {
    daemon_id: String,
    request_id: String,
    session_id: String,
    sdp: String,
    binding: serde_json::Value,
}

#[derive(Debug, Serialize)]
struct BrowserAnswerResponse {
    ok: bool,
    session_id: String,
    sdp: String,
    binding: serde_json::Value,
    daemon_public_key: String,
    session_grant: String,
}

async fn daemon_answer(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<DaemonAnswerRequest>,
) -> ApiResult<Json<serde_json::Value>> {
    require_daemon_auth(&state, &headers)?;
    let pending = state
        .pending_offers
        .lock()
        .await
        .remove(body.request_id.trim())
        .ok_or_else(|| ApiError::not_found("offer not found"))?;
    if pending.daemon_id != body.daemon_id {
        let _ = pending
            .response_tx
            .send(Err("daemon_id mismatch in answer".to_string()));
        return Err(ApiError::bad_request("daemon_id mismatch"));
    }
    let validation_error = validate_dashboard_binding(
        &body.binding,
        &pending.daemon_public_key,
        &pending.session_grant,
    );
    if let Err(error) = validation_error {
        let _ = pending.response_tx.send(Err(error.clone()));
        return Err(ApiError::bad_request(error));
    }
    let answer_session_id = body.session_id.trim().to_string();
    if answer_session_id.is_empty() {
        let _ = pending
            .response_tx
            .send(Err("daemon answer missing session_id".to_string()));
        return Err(ApiError::bad_request("daemon answer missing session_id"));
    }
    record_active_dashboard_session(&state, &pending.daemon_id, &answer_session_id).await;
    let answer = BrowserAnswerResponse {
        ok: true,
        session_id: answer_session_id.clone(),
        sdp: body.sdp,
        binding: body.binding,
        daemon_public_key: pending.daemon_public_key,
        session_grant: pending.session_grant,
    };
    let _ = pending.response_tx.send(Ok(answer));
    {
        let mut store = state.store.lock().await;
        audit(
            &mut store,
            "dashboard_grant_answered",
            Some(pending.user_id),
            Some(pending.daemon_id),
            json!({ "request_id": body.request_id, "session_id": answer_session_id }),
        );
        persist_locked(&state, &store)?;
    }
    Ok(Json(json!({ "ok": true })))
}

fn validate_dashboard_binding(
    binding: &serde_json::Value,
    daemon_public_key: &str,
    session_grant: &str,
) -> Result<(), String> {
    let binding_key = binding
        .get("daemon_public_key")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if binding_key != daemon_public_key {
        return Err("binding daemon_public_key mismatch".to_string());
    }
    let grant_hash = binding
        .get("session_grant_sha256")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let expected = sha256_b64u(session_grant.as_bytes());
    if grant_hash != expected {
        return Err("binding session_grant_sha256 mismatch".to_string());
    }
    Ok(())
}

#[derive(Debug, Deserialize)]
struct DaemonErrorRequest {
    daemon_id: String,
    request_id: String,
    error: String,
}

async fn daemon_error(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<DaemonErrorRequest>,
) -> ApiResult<Json<serde_json::Value>> {
    require_daemon_auth(&state, &headers)?;
    if let Some(pending) = state
        .pending_offers
        .lock()
        .await
        .remove(body.request_id.trim())
    {
        if pending.daemon_id == body.daemon_id {
            let _ = pending.response_tx.send(Err(body.error));
        }
    }
    Ok(Json(json!({ "ok": true })))
}

#[derive(Debug, Deserialize)]
struct AckRequest {
    daemon_id: String,
    request_id: String,
    ok: bool,
}

async fn daemon_ack(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<AckRequest>,
) -> ApiResult<Json<serde_json::Value>> {
    require_daemon_auth(&state, &headers)?;
    let _ = (body.daemon_id, body.request_id, body.ok);
    Ok(Json(json!({ "ok": true })))
}

#[derive(Debug, Deserialize)]
struct ClaimProofRequest {
    daemon_id: String,
    request_id: String,
    claim_id: String,
    challenge: String,
    signature: String,
}

async fn daemon_claim_proof(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<ClaimProofRequest>,
) -> ApiResult<Json<serde_json::Value>> {
    require_daemon_auth(&state, &headers)?;
    let pending = state
        .pending_claims
        .lock()
        .await
        .get(body.claim_id.trim())
        .cloned()
        .ok_or_else(|| ApiError::not_found("claim not found"))?;
    if pending.daemon_id != body.daemon_id || pending.challenge != body.challenge {
        reject_claim(&state, &body.claim_id, "claim proof mismatch").await;
        return Err(ApiError::bad_request("claim proof mismatch"));
    }
    if !matches!(pending.status, ClaimStatus::Pending) {
        return Err(ApiError::bad_request("claim is already resolved"));
    }
    if now_unix_ms().saturating_sub(pending.created_unix_ms) > CLAIM_TIMEOUT_MS {
        reject_claim(&state, &body.claim_id, "claim timed out").await;
        return Err(ApiError::bad_request("claim timed out"));
    }
    let daemon = {
        let store = state.store.lock().await;
        store
            .daemons
            .iter()
            .find(|d| d.daemon_id == body.daemon_id)
            .cloned()
            .ok_or_else(|| ApiError::not_found("daemon not found"))?
    };
    let payload = claim_signing_payload(
        &body.claim_id,
        &body.daemon_id,
        &daemon.daemon_public_key,
        &body.challenge,
    );
    if !verify_ed25519_b64u(
        &daemon.daemon_public_key,
        payload.as_bytes(),
        body.signature.trim(),
    ) {
        reject_claim(&state, &body.claim_id, "claim signature invalid").await;
        return Err(ApiError::bad_request("claim signature invalid"));
    }
    {
        let mut store = state.store.lock().await;
        let daemon = store
            .daemons
            .iter_mut()
            .find(|d| d.daemon_id == body.daemon_id)
            .ok_or_else(|| ApiError::not_found("daemon not found"))?;
        daemon.owner_user_id = Some(pending.user_id);
        daemon.claim_code_hash = None;
        daemon.claim_code_created_unix_ms = None;
        daemon.updated_unix_ms = now_unix_ms();
        audit(
            &mut store,
            "daemon_claimed",
            Some(pending.user_id),
            Some(body.daemon_id.clone()),
            json!({ "claim_id": body.claim_id, "request_id": body.request_id }),
        );
        persist_locked(&state, &store)?;
    }
    state.claim_codes.lock().await.remove(&body.daemon_id);
    {
        let mut claims = state.pending_claims.lock().await;
        if let Some(claim) = claims.get_mut(body.claim_id.trim()) {
            claim.status = ClaimStatus::Approved {
                daemon_id: body.daemon_id.clone(),
            };
        }
    }
    Ok(Json(json!({ "ok": true })))
}

async fn reject_claim(state: &AppState, claim_id: &str, error: &str) {
    let mut claims = state.pending_claims.lock().await;
    if let Some(claim) = claims.get_mut(claim_id.trim()) {
        claim.status = ClaimStatus::Rejected {
            error: error.to_string(),
        };
    }
}

fn claim_signing_payload(
    claim_id: &str,
    daemon_id: &str,
    daemon_public_key: &str,
    challenge: &str,
) -> String {
    format!("{CLAIM_PROTOCOL}\n{claim_id}\n{daemon_id}\n{daemon_public_key}\n{challenge}\n")
}

fn verify_ed25519_b64u(public_key_b64u: &str, payload: &[u8], signature_b64u: &str) -> bool {
    let Ok(public_key) = b64u_decode(public_key_b64u) else {
        return false;
    };
    let Ok(signature) = b64u_decode(signature_b64u) else {
        return false;
    };
    ring::signature::UnparsedPublicKey::new(&ring::signature::ED25519, public_key)
        .verify(payload, &signature)
        .is_ok()
}

#[derive(Debug, Deserialize)]
struct BrowserOfferRequest {
    daemon_id: String,
    sdp: String,
    #[serde(default)]
    client_nonce: Option<String>,
}

async fn browser_offer(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<BrowserOfferRequest>,
) -> ApiResult<Response> {
    let user = require_user(&state, &headers).await?;
    require_csrf(&state, &headers).await?;
    check_rate_limit(&state, &headers, "browser_offer", 60, 60_000).await?;
    let daemon_id = body.daemon_id.trim().to_string();
    let sdp = body.sdp;
    if daemon_id.is_empty() || sdp.trim().is_empty() {
        return Err(ApiError::bad_request("daemon_id and sdp are required"));
    }
    let daemon = {
        let store = state.store.lock().await;
        store
            .daemons
            .iter()
            .find(|d| d.daemon_id == daemon_id && d.owner_user_id == Some(user.id))
            .cloned()
            .ok_or_else(|| ApiError::not_found("daemon not found"))?
    };
    let request_id = Uuid::new_v4().to_string();
    let session_grant = random_b64u(32);
    let (tx, rx) = oneshot::channel();
    state.pending_offers.lock().await.insert(
        request_id.clone(),
        PendingOffer {
            daemon_id: daemon_id.clone(),
            user_id: user.id,
            daemon_public_key: daemon.daemon_public_key.clone(),
            session_grant: session_grant.clone(),
            response_tx: tx,
        },
    );
    enqueue_event(
        &state,
        &daemon_id,
        RendezvousEvent {
            id: request_id.clone(),
            kind: "offer".to_string(),
            sdp: Some(sdp),
            session_grant: Some(session_grant),
            client_nonce: body
                .client_nonce
                .as_deref()
                .map(str::trim)
                .filter(|v| !v.is_empty())
                .map(str::to_string),
            user_id: Some(user.id.to_string()),
            account_name: Some(user.account_name.clone()),
            ..RendezvousEvent::default()
        },
    )
    .await;
    {
        let mut store = state.store.lock().await;
        audit(
            &mut store,
            "dashboard_grant_started",
            Some(user.id),
            Some(daemon_id.clone()),
            json!({ "request_id": request_id }),
        );
        persist_locked(&state, &store)?;
    }
    match tokio::time::timeout(Duration::from_millis(OFFER_TIMEOUT_MS), rx).await {
        Ok(Ok(Ok(answer))) => Ok(Json(answer).into_response()),
        Ok(Ok(Err(error))) => Err(ApiError::new(StatusCode::BAD_GATEWAY, error)),
        Ok(Err(_)) => Err(ApiError::new(
            StatusCode::BAD_GATEWAY,
            "daemon answer channel closed",
        )),
        Err(_) => {
            state.pending_offers.lock().await.remove(&request_id);
            Err(ApiError::new(
                StatusCode::GATEWAY_TIMEOUT,
                "timed out waiting for daemon answer",
            ))
        }
    }
}

#[derive(Debug, Deserialize)]
struct BrowserIceRequest {
    daemon_id: String,
    session_id: String,
    #[serde(default)]
    candidate: serde_json::Value,
}

async fn browser_ice(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<BrowserIceRequest>,
) -> ApiResult<Json<serde_json::Value>> {
    let user = require_user(&state, &headers).await?;
    require_csrf(&state, &headers).await?;
    check_rate_limit(&state, &headers, "browser_ice", 600, 60_000).await?;
    require_owned_daemon(&state, user.id, &body.daemon_id).await?;
    enqueue_event(
        &state,
        body.daemon_id.trim(),
        RendezvousEvent {
            id: Uuid::new_v4().to_string(),
            kind: "ice".to_string(),
            session_id: Some(body.session_id),
            candidate: Some(body.candidate),
            ..RendezvousEvent::default()
        },
    )
    .await;
    Ok(Json(json!({ "ok": true })))
}

#[derive(Debug, Deserialize)]
struct BrowserCloseRequest {
    daemon_id: String,
    session_id: String,
}

async fn browser_close(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<BrowserCloseRequest>,
) -> ApiResult<Json<serde_json::Value>> {
    let user = require_user(&state, &headers).await?;
    require_csrf(&state, &headers).await?;
    require_owned_daemon(&state, user.id, &body.daemon_id).await?;
    state
        .active_sessions
        .lock()
        .await
        .remove(body.session_id.trim());
    enqueue_event(
        &state,
        body.daemon_id.trim(),
        RendezvousEvent {
            id: Uuid::new_v4().to_string(),
            kind: "close".to_string(),
            session_id: Some(body.session_id),
            ..RendezvousEvent::default()
        },
    )
    .await;
    Ok(Json(json!({ "ok": true })))
}

async fn require_owned_daemon(
    state: &AppState,
    user_id: Uuid,
    daemon_id: &str,
) -> ApiResult<DaemonRecord> {
    ensure_owned_daemon(state, user_id, daemon_id).await?;
    let store = state.store.lock().await;
    store
        .daemons
        .iter()
        .find(|d| d.daemon_id == daemon_id.trim() && d.owner_user_id == Some(user_id))
        .cloned()
        .ok_or_else(|| ApiError::not_found("daemon not found"))
}

async fn ensure_owned_daemon(state: &AppState, user_id: Uuid, daemon_id: &str) -> ApiResult<()> {
    let daemon_id = daemon_id.trim();
    let store = state.store.lock().await;
    let daemon = store
        .daemons
        .iter()
        .find(|d| d.daemon_id == daemon_id)
        .ok_or_else(|| ApiError::not_found("daemon not found"))?;
    if daemon.owner_user_id == Some(user_id) {
        Ok(())
    } else {
        Err(ApiError::forbidden("daemon belongs to a different account"))
    }
}

fn connect_ui_html(origin: &str, product_title: &str, account_subtitle: &str) -> String {
    format!(
        r#"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <title>{product_title}</title>
  <style>
    :root {{
      color-scheme: dark;
      --bg: #0d1016;
      --top: #121722;
      --surface: #161b24;
      --surface-2: #1c2330;
      --surface-3: #222b38;
      --line: #2b3443;
      --line-strong: #3d4858;
      --text: #f5f7fb;
      --muted: #9da8b7;
      --muted-2: #727f8f;
      --accent: #69b7ff;
      --accent-hover: #88c8ff;
      --accent-ink: #061320;
      --ok: #7edc8f;
      --warn: #ffd166;
      --err: #ff7d9a;
      --focus: #f2c94c;
      --shadow: 0 18px 50px rgba(0, 0, 0, .28);
      font-family: ui-sans-serif, system-ui, -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif;
      background: var(--bg);
      color: var(--text);
    }}
    * {{ box-sizing: border-box; }}
    html {{ min-height: 100%; }}
    body {{ margin: 0; min-height: 100vh; background: var(--bg); }}
    button, input {{ font: inherit; }}
    button {{ height: 38px; padding: 0 14px; color: var(--accent-ink); background: var(--accent); border: 1px solid transparent; border-radius: 7px; font-weight: 700; cursor: pointer; transition: background .16s ease, border-color .16s ease, color .16s ease, transform .12s ease; white-space: nowrap; }}
    button:hover:not(:disabled) {{ background: var(--accent-hover); transform: translateY(-1px); }}
    button:focus-visible, input:focus-visible {{ outline: 2px solid var(--focus); outline-offset: 2px; }}
    button.secondary {{ color: var(--text); background: var(--surface-2); border-color: var(--line-strong); }}
    button.secondary:hover:not(:disabled) {{ background: var(--surface-3); }}
    button.ghost {{ color: var(--muted); background: transparent; border-color: var(--line); }}
    button.ghost:hover:not(:disabled) {{ color: var(--text); background: var(--surface-2); }}
    button.danger {{ color: var(--err); background: rgba(255, 125, 154, .08); border-color: rgba(255, 125, 154, .58); }}
    button.danger:hover:not(:disabled) {{ background: rgba(255, 125, 154, .15); }}
    button:disabled {{ opacity: .58; cursor: default; transform: none; }}
    input {{ width: 100%; min-width: 0; height: 42px; padding: 9px 12px; color: var(--text); background: #10151d; border: 1px solid var(--line-strong); border-radius: 7px; }}
    input::placeholder {{ color: var(--muted-2); }}
    header {{ border-bottom: 1px solid var(--line); background: var(--top); }}
    .topbar {{ width: min(1180px, calc(100vw - 32px)); margin: 0 auto; min-height: 72px; display: flex; align-items: center; justify-content: space-between; gap: 18px; }}
    .brand {{ display: flex; align-items: center; gap: 12px; min-width: 0; }}
    .brand-mark {{ width: 36px; height: 36px; display: grid; place-items: center; flex: 0 0 auto; border: 1px solid var(--line-strong); border-radius: 8px; color: var(--accent); background: #101722; font-size: 13px; font-weight: 800; letter-spacing: 0; }}
    .brand h1 {{ font-size: 20px; line-height: 1.15; margin: 0; letter-spacing: 0; }}
    .origin-chip {{ min-width: 0; display: flex; align-items: center; gap: 8px; color: var(--muted); font-size: 12px; }}
    .origin-chip code {{ max-width: min(48vw, 420px); overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }}
    main.shell {{ width: min(1180px, calc(100vw - 32px)); margin: 0 auto; padding: 22px 0 44px; display: grid; grid-template-columns: minmax(280px, 340px) minmax(0, 1fr); gap: 16px; align-items: start; }}
    body.signed-out main.shell {{ width: min(440px, calc(100vw - 32px)); grid-template-columns: 1fr; padding-top: 54px; }}
    section {{ min-width: 0; border: 1px solid var(--line); background: var(--surface); border-radius: 8px; box-shadow: var(--shadow); }}
    .panel-header {{ min-height: 62px; padding: 16px 18px; border-bottom: 1px solid var(--line); display: flex; align-items: center; justify-content: space-between; gap: 14px; }}
    .panel-title {{ min-width: 0; }}
    h2 {{ font-size: 15px; line-height: 1.25; margin: 0; letter-spacing: 0; }}
    .sub {{ color: var(--muted); font-size: 13px; line-height: 1.35; margin-top: 4px; }}
    .panel-body {{ padding: 18px; }}
    .stack {{ display: grid; gap: 14px; }}
    .row {{ display: flex; gap: 9px; align-items: center; flex-wrap: wrap; }}
    .split {{ display: flex; justify-content: space-between; gap: 12px; align-items: center; }}
    label {{ display: block; color: var(--muted); font-size: 12px; font-weight: 700; margin-bottom: 7px; }}
    .actions {{ display: flex; gap: 8px; align-items: center; flex-wrap: wrap; }}
    .account-summary {{ padding-top: 14px; border-top: 1px solid var(--line); }}
    .handle {{ color: var(--text); font-size: 17px; font-weight: 750; overflow-wrap: anywhere; }}
    .metric-row {{ display: flex; gap: 8px; align-items: center; flex-wrap: wrap; margin-top: 10px; }}
    .claim-strip {{ display: grid; grid-template-columns: minmax(220px, 1fr) auto; gap: 9px; align-items: end; padding-bottom: 16px; border-bottom: 1px solid var(--line); }}
    .status {{ min-height: 20px; color: var(--muted); font-size: 13px; line-height: 1.35; overflow-wrap: anywhere; }}
    .status.status-ok {{ color: var(--ok); }}
    .status.status-err {{ color: var(--err); }}
    .status.status-warn {{ color: var(--warn); }}
    .table-wrap {{ overflow-x: auto; }}
    table {{ width: 100%; border-collapse: collapse; font-size: 13px; }}
    th, td {{ text-align: left; padding: 13px 10px; border-bottom: 1px solid var(--line); vertical-align: middle; }}
    th {{ color: var(--muted); font-size: 11px; font-weight: 800; text-transform: uppercase; letter-spacing: .04em; }}
    tbody tr:hover {{ background: rgba(255, 255, 255, .025); }}
    tbody tr:last-child td {{ border-bottom: 0; }}
    code {{ color: var(--muted); font-family: ui-monospace, SFMono-Regular, Menlo, Consolas, monospace; overflow-wrap: anywhere; }}
    .daemon-name {{ display: grid; gap: 3px; min-width: 220px; }}
    .daemon-name strong {{ font-size: 14px; }}
    .daemon-activity {{ display: grid; gap: 6px; min-width: 116px; }}
    .target-route {{ display: grid; gap: 4px; min-width: 180px; }}
    .action-cell .actions {{ justify-content: flex-end; flex-wrap: nowrap; }}
    .pill {{ display: inline-flex; align-items: center; width: fit-content; min-height: 24px; padding: 0 9px; border-radius: 999px; background: var(--surface-2); color: var(--muted); border: 1px solid var(--line); font-size: 12px; font-weight: 750; }}
    .pill.ok {{ color: var(--ok); border-color: rgba(126, 220, 143, .4); background: rgba(126, 220, 143, .09); }}
    .pill.warn {{ color: var(--warn); border-color: rgba(255, 209, 102, .35); background: rgba(255, 209, 102, .08); }}
    .empty-state {{ padding: 22px 10px; color: var(--muted); }}
    .hidden {{ display: none !important; }}
    .wide {{ grid-column: 1 / -1; }}
    .audit {{ display: grid; gap: 0; }}
    .event {{ padding: 13px 0; border-bottom: 1px solid var(--line); font-size: 13px; }}
    .event:first-child {{ padding-top: 0; }}
    .event:last-child {{ border-bottom: 0; padding-bottom: 0; }}
    .event-line {{ display: flex; justify-content: space-between; gap: 12px; align-items: baseline; }}
    .event-name {{ font-weight: 750; }}
    .event time {{ color: var(--muted); font-size: 12px; white-space: nowrap; }}
    .event code {{ display: inline-block; margin-top: 4px; }}
    @media (max-width: 820px) {{
      .topbar, main.shell {{ width: min(100vw - 24px, 680px); }}
      .topbar {{ min-height: auto; padding: 14px 0; align-items: flex-start; }}
      main.shell {{ grid-template-columns: 1fr; padding-top: 14px; }}
      .origin-chip {{ display: none; }}
      .claim-strip {{ grid-template-columns: 1fr; }}
      .claim-strip button {{ width: 100%; }}
      table, thead, tbody, tr, th, td {{ display: block; width: 100%; }}
      thead {{ display: none; }}
      tr {{ padding: 12px 0; border-bottom: 1px solid var(--line); }}
      tr:last-child {{ border-bottom: 0; }}
      td {{ border-bottom: 0; padding: 7px 0; }}
      td::before {{ content: attr(data-cell-label); display: block; color: var(--muted-2); font-size: 11px; font-weight: 800; text-transform: uppercase; letter-spacing: .04em; margin-bottom: 4px; }}
      .action-cell::before {{ display: none; }}
      .action-cell .actions {{ justify-content: stretch; flex-wrap: wrap; }}
      .action-cell button {{ flex: 1 1 92px; }}
    }}
  </style>
</head>
<body class="signed-out">
  <header>
    <div class="topbar">
      <div class="brand">
        <div class="brand-mark" aria-hidden="true">IC</div>
        <div>
        <h1>{product_title}</h1>
          <div class="origin-chip"><span>Origin</span><code>{origin}</code></div>
        </div>
      </div>
      <button id="logout" class="ghost hidden">Sign out</button>
    </div>
  </header>
  <main class="shell">
    <section id="auth">
      <div class="panel-header">
        <div class="panel-title">
          <h2>Account</h2>
          <div class="sub">{account_subtitle}</div>
        </div>
      </div>
      <div class="panel-body stack">
        <div>
          <label for="account">Account handle</label>
          <input id="account" autocomplete="username webauthn" autocapitalize="none" spellcheck="false" placeholder="user">
        </div>
        <div id="auth-actions" class="actions">
          <button id="login">Sign in</button>
          <button id="register" class="secondary">Create passkey</button>
        </div>
        <div id="session-card" class="account-summary hidden">
          <div id="session-handle" class="handle"></div>
          <div class="metric-row">
            <span id="session-passkeys" class="pill"></span>
            <span class="pill ok">active</span>
          </div>
        </div>
        <div id="auth-status" class="status"></div>
      </div>
    </section>

    <section id="manage" class="hidden">
      <div class="panel-header">
        <div class="panel-title">
          <h2>Daemons</h2>
          <div id="who" class="sub"></div>
        </div>
        <button id="refresh" class="secondary">Refresh</button>
      </div>
      <div class="panel-body stack">
        <div class="claim-strip">
          <div>
            <label for="claim-code">Claim phrase</label>
            <input id="claim-code" autocomplete="off" spellcheck="false" placeholder="12-word claim phrase">
          </div>
          <button id="claim">Claim</button>
        </div>
        <div id="claim-status" class="status"></div>
        <div class="table-wrap">
          <table>
            <thead><tr><th>Daemon</th><th>Activity</th><th>Public key</th><th></th></tr></thead>
            <tbody id="daemon-rows"></tbody>
          </table>
        </div>
      </div>
    </section>

    <section id="fleet-section" class="wide hidden">
      <div class="panel-header">
        <div class="panel-title">
          <h2>Access Targets</h2>
          <div class="sub">Account-backed fleet navigation; each daemon still enforces its own access locally</div>
        </div>
      </div>
      <div class="panel-body">
        <div class="table-wrap">
          <table>
            <thead><tr><th>Target</th><th>Route</th><th>Authority</th><th></th></tr></thead>
            <tbody id="fleet-rows"></tbody>
          </table>
        </div>
      </div>
    </section>

    <section id="audit-section" class="wide hidden">
      <div class="panel-header">
        <div class="panel-title">
          <h2>Audit</h2>
          <div class="sub">Recent account activity</div>
        </div>
      </div>
      <div class="panel-body">
        <div id="audit" class="audit"></div>
      </div>
    </section>
  </main>
<script>
const $ = id => document.getElementById(id);
const state = {{ user: null, daemons: [], fleetTargets: [], csrfToken: '' }};

function setStatus(id, text, kind = '') {{
  const el = $(id);
  el.textContent = text || '';
  el.className = 'status' + (kind ? ' status-' + kind : '');
}}

function setBusy(id, busy) {{
  const el = $(id);
  if (!el) return;
  el.disabled = Boolean(busy);
}}

async function api(path, options = {{}}) {{
  const headers = {{
    'content-type': 'application/json',
    ...(options.headers || {{}}),
  }};
  if (state.csrfToken && !headers['x-intendant-csrf']) {{
    headers['x-intendant-csrf'] = state.csrfToken;
  }}
  const resp = await fetch(path, {{
    ...options,
    headers,
  }});
  const body = await resp.json().catch(() => ({{}}));
  if (!resp.ok || body.ok === false) throw new Error(body.error || `HTTP ${{resp.status}}`);
  return body;
}}

function b64uToBuf(value) {{
  const text = String(value || '').replace(/-/g, '+').replace(/_/g, '/');
  const padded = text.padEnd(Math.ceil(text.length / 4) * 4, '=');
  const bin = atob(padded);
  const out = new Uint8Array(bin.length);
  for (let i = 0; i < bin.length; i += 1) out[i] = bin.charCodeAt(i);
  return out.buffer;
}}

function bufToB64u(value) {{
  const bytes = new Uint8Array(value || new ArrayBuffer(0));
  let bin = '';
  for (const b of bytes) bin += String.fromCharCode(b);
  return btoa(bin).replace(/\+/g, '-').replace(/\//g, '_').replace(/=+$/g, '');
}}

function publicKeyOptions(start) {{
  const options = start.options && (start.options.publicKey || start.options);
  if (!options) throw new Error('missing WebAuthn options');
  options.challenge = b64uToBuf(options.challenge);
  if (options.user?.id) options.user.id = b64uToBuf(options.user.id);
  for (const cred of options.excludeCredentials || []) cred.id = b64uToBuf(cred.id);
  for (const cred of options.allowCredentials || []) cred.id = b64uToBuf(cred.id);
  return options;
}}

function registrationCredentialJSON(credential) {{
  return {{
    id: credential.id,
    clientDataJSON: bufToB64u(credential.response.clientDataJSON),
    attestationObject: bufToB64u(credential.response.attestationObject),
    transports: credential.response.getTransports ? credential.response.getTransports() : [],
  }};
}}

function authenticationCredentialJSON(credential) {{
  return {{
    id: credential.id,
    clientDataJSON: bufToB64u(credential.response.clientDataJSON),
    authenticatorData: bufToB64u(credential.response.authenticatorData),
    signature: bufToB64u(credential.response.signature),
    userHandle: credential.response.userHandle ? bufToB64u(credential.response.userHandle) : null,
  }};
}}

async function createPasskey() {{
  const account = $('account').value.trim();
  if (!account) throw new Error('Account handle is required');
  setBusy('register', true);
  setStatus('auth-status', 'Waiting for passkey', '');
  try {{
    const start = await api('/api/auth/register/start', {{
      method: 'POST',
      body: JSON.stringify({{ account_name: account }}),
    }});
    const credential = await navigator.credentials.create({{ publicKey: publicKeyOptions(start) }});
    const done = await api('/api/auth/register/finish', {{
      method: 'POST',
      body: JSON.stringify({{
        flow_id: start.flow_id,
        credential: registrationCredentialJSON(credential),
      }}),
    }});
    state.user = done.user;
    state.csrfToken = done.csrf_token || state.csrfToken;
    setStatus('auth-status', 'Signed in', 'ok');
    await refreshAll();
  }} finally {{
    setBusy('register', false);
  }}
}}

async function login() {{
  const account = $('account').value.trim();
  if (!account) throw new Error('Account handle is required');
  setBusy('login', true);
  setStatus('auth-status', 'Waiting for passkey', '');
  try {{
    const start = await api('/api/auth/login/start', {{
      method: 'POST',
      body: JSON.stringify({{ account_name: account }}),
    }});
    const credential = await navigator.credentials.get({{ publicKey: publicKeyOptions(start) }});
    const done = await api('/api/auth/login/finish', {{
      method: 'POST',
      body: JSON.stringify({{
        flow_id: start.flow_id,
        credential: authenticationCredentialJSON(credential),
      }}),
    }});
    state.user = done.user;
    state.csrfToken = done.csrf_token || state.csrfToken;
    setStatus('auth-status', 'Signed in', 'ok');
    await refreshAll();
  }} finally {{
    setBusy('login', false);
  }}
}}

async function claimDaemon() {{
  const claimCode = $('claim-code').value.trim();
  if (!claimCode) throw new Error('Claim phrase is required');
  setBusy('claim', true);
  setStatus('claim-status', 'Waiting for daemon proof', '');
  try {{
    const start = await api('/api/claims/claim', {{
      method: 'POST',
      body: JSON.stringify({{ claim_code: claimCode }}),
    }});
    const deadline = Date.now() + 65000;
    while (Date.now() < deadline) {{
      await new Promise(resolve => setTimeout(resolve, 750));
      const status = await api(`/api/claims/${{encodeURIComponent(start.claim_id)}}`);
      if (status.result?.status === 'approved') {{
        setStatus('claim-status', `Claimed ${{status.result.daemon_id}}`, 'ok');
        $('claim-code').value = '';
        await refreshAll();
        return;
      }}
      if (status.result?.status === 'rejected') {{
        throw new Error(status.result.error || 'claim rejected');
      }}
    }}
    throw new Error('claim timed out');
  }} finally {{
    setBusy('claim', false);
  }}
}}

async function refreshAll() {{
  setBusy('refresh', true);
  try {{
    const me = await api('/api/me');
    state.csrfToken = me.csrf_token || '';
    state.user = me.authenticated ? me.user : null;
    renderAuth();
    if (!state.user) return;
    const [daemons, fleet, audit] = await Promise.all([
      api('/api/daemons'),
      api('/api/fleet/targets'),
      api('/api/audit'),
    ]);
    state.daemons = daemons.daemons || [];
    state.fleetTargets = fleet.targets || [];
    renderDaemons();
    renderFleetTargets();
    renderAudit(audit.events || []);
  }} finally {{
    setBusy('refresh', false);
  }}
}}

function renderAuth() {{
  const authed = Boolean(state.user);
  document.body.classList.toggle('signed-out', !authed);
  document.body.classList.toggle('signed-in', authed);
  $('manage').classList.toggle('hidden', !authed);
  $('fleet-section').classList.toggle('hidden', !authed);
  $('audit-section').classList.toggle('hidden', !authed);
  $('logout').classList.toggle('hidden', !authed);
  $('auth-actions').classList.toggle('hidden', authed);
  $('session-card').classList.toggle('hidden', !authed);
  $('account').disabled = authed;
  if (authed) {{
    $('account').value = state.user.account_name || '';
    $('session-handle').textContent = '@' + state.user.account_name;
    $('session-passkeys').textContent = `${{state.user.passkey_count}} passkey${{state.user.passkey_count === 1 ? '' : 's'}}`;
    $('who').textContent = '@' + state.user.account_name;
  }} else {{
    $('session-handle').textContent = '';
    $('session-passkeys').textContent = '';
    $('who').textContent = '';
  }}
}}

function renderDaemons() {{
  const rows = $('daemon-rows');
  rows.innerHTML = '';
  if (state.daemons.length === 0) {{
    rows.innerHTML = '<tr><td colspan="4" class="empty-state">No claimed daemons</td></tr>';
    return;
  }}
  for (const daemon of state.daemons) {{
    const tr = document.createElement('tr');
    const key = String(daemon.daemon_public_key || '');
    const label = String(daemon.label || daemon.daemon_id || '');
    const lastSeen = formatRelative(daemon.last_seen_unix_ms);
    tr.innerHTML = `
      <td data-cell-label="Daemon"><div class="daemon-name"><strong>${{escapeHtml(label)}}</strong><code>${{escapeHtml(daemon.daemon_id)}}</code></div></td>
      <td data-cell-label="Activity"><div class="daemon-activity"><span class="pill ${{daemon.online ? 'ok' : 'warn'}}">${{daemon.online ? 'online' : 'idle'}}</span><span class="sub">${{escapeHtml(lastSeen)}}</span></div></td>
      <td data-cell-label="Public key"><code title="${{escapeAttr(key)}}">${{escapeHtml(compactKey(key))}}</code></td>
      <td class="action-cell" data-cell-label="Actions"><div class="actions">
        <button data-open="${{escapeAttr(daemon.daemon_id)}}">Open</button>
        <button class="secondary" data-rename="${{escapeAttr(daemon.daemon_id)}}">Rename</button>
        <button class="danger" data-revoke="${{escapeAttr(daemon.daemon_id)}}">Revoke</button>
      </div></td>`;
    rows.appendChild(tr);
  }}
  rows.querySelectorAll('[data-open]').forEach(button => {{
    button.addEventListener('click', () => {{
      const id = button.getAttribute('data-open');
      window.location.href = `/app?connect=1&daemon_id=${{encodeURIComponent(id)}}`;
    }});
  }});
  rows.querySelectorAll('[data-revoke]').forEach(button => {{
    button.addEventListener('click', async () => {{
      const id = button.getAttribute('data-revoke');
      if (!confirm(`Revoke access to ${{id}}?`)) return;
      await api(`/api/daemons/${{encodeURIComponent(id)}}/revoke`, {{ method: 'POST', body: '{{}}' }});
      await refreshAll();
    }});
  }});
  rows.querySelectorAll('[data-rename]').forEach(button => {{
    button.addEventListener('click', async () => {{
      const id = button.getAttribute('data-rename');
      const daemon = state.daemons.find(item => item.daemon_id === id) || {{}};
      const next = prompt('Name this daemon', daemon.label || daemon.daemon_id || '');
      if (next === null) return;
      await api(`/api/daemons/${{encodeURIComponent(id)}}/label`, {{
        method: 'POST',
        body: JSON.stringify({{ label: next }}),
      }});
      await refreshAll();
    }});
  }});
}}

function renderFleetTargets() {{
  const rows = $('fleet-rows');
  rows.innerHTML = '';
  if (state.fleetTargets.length === 0) {{
    rows.innerHTML = '<tr><td colspan="4" class="empty-state">No access targets</td></tr>';
    return;
  }}
  for (const target of state.fleetTargets) {{
    const tr = document.createElement('tr');
    const id = String(target.host_id || target.id || '');
    const label = String(target.label || id || 'Target');
    const source = String(target.source || 'browser_fleet');
    const route = String(target.route_label || target.route || target.url || 'Remembered route');
    const auth = String(target.auth_label || target.auth || target.effective_role_label || 'Account record');
    const statusClass = target.online || target.connected ? 'ok' : 'warn';
    const statusText = target.online || target.connected ? 'online' : 'remembered';
    const url = String(target.url || '');
    const canForget = target.claimed_daemon !== true;
    tr.innerHTML = `
      <td data-cell-label="Target"><div class="daemon-name"><strong>${{escapeHtml(label)}}</strong><code>${{escapeHtml(id)}}</code></div></td>
      <td data-cell-label="Route"><div class="target-route"><span class="pill ${{statusClass}}">${{escapeHtml(statusText)}}</span><span class="sub">${{escapeHtml(route)}}</span></div></td>
      <td data-cell-label="Authority"><div class="target-route"><span class="pill">${{escapeHtml(source.replaceAll('_', ' '))}}</span><span class="sub">${{escapeHtml(auth)}}</span></div></td>
      <td class="action-cell" data-cell-label="Actions"><div class="actions">
        <button data-fleet-open="${{escapeAttr(url)}}" ${{url ? '' : 'disabled'}}>Open</button>
        <button class="secondary" data-fleet-forget="${{escapeAttr(id)}}" ${{canForget ? '' : 'disabled'}}>Forget</button>
      </div></td>`;
    rows.appendChild(tr);
  }}
  rows.querySelectorAll('[data-fleet-open]').forEach(button => {{
    button.addEventListener('click', () => {{
      const url = button.getAttribute('data-fleet-open');
      if (url) window.location.href = url;
    }});
  }});
  rows.querySelectorAll('[data-fleet-forget]').forEach(button => {{
    button.addEventListener('click', async () => {{
      const id = button.getAttribute('data-fleet-forget');
      if (!id) return;
      await api(`/api/fleet/targets/${{encodeURIComponent(id)}}/forget`, {{ method: 'POST', body: '{{}}' }});
      await refreshAll();
    }});
  }});
}}

function renderAudit(events) {{
  const el = $('audit');
  el.innerHTML = '';
  if (!events.length) {{
    el.innerHTML = '<div class="empty-state">No audit events</div>';
    return;
  }}
  for (const event of events.slice(0, 30)) {{
    const div = document.createElement('div');
    div.className = 'event';
    const date = formatDate(event.unix_ms);
    const name = String(event.event || '').replaceAll('_', ' ');
    div.innerHTML = `<div class="event-line"><span class="event-name">${{escapeHtml(name)}}</span><time>${{escapeHtml(date)}}</time></div><code>${{escapeHtml(event.daemon_id || '')}}</code>`;
    el.appendChild(div);
  }}
}}

function compactKey(value) {{
  const key = String(value || '');
  if (key.length <= 24) return key;
  return key.slice(0, 12) + '...' + key.slice(-8);
}}

function formatDate(unixMs) {{
  const value = Number(unixMs || 0);
  if (!value) return 'unknown';
  return new Date(value).toLocaleString();
}}

function formatRelative(unixMs) {{
  const value = Number(unixMs || 0);
  if (!value) return 'never seen';
  const seconds = Math.max(0, Math.floor((Date.now() - value) / 1000));
  if (seconds < 10) return 'just now';
  if (seconds < 60) return `${{seconds}}s ago`;
  const minutes = Math.floor(seconds / 60);
  if (minutes < 60) return `${{minutes}}m ago`;
  const hours = Math.floor(minutes / 60);
  if (hours < 48) return `${{hours}}h ago`;
  return `${{Math.floor(hours / 24)}}d ago`;
}}

function escapeHtml(value) {{
  return String(value ?? '').replace(/[&<>"']/g, c => ({{'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'}}[c]));
}}
function escapeAttr(value) {{ return escapeHtml(value); }}

$('register').addEventListener('click', () => createPasskey().catch(err => setStatus('auth-status', err.message, 'err')));
$('login').addEventListener('click', () => login().catch(err => setStatus('auth-status', err.message, 'err')));
$('claim').addEventListener('click', () => claimDaemon().catch(err => setStatus('claim-status', err.message, 'err')));
$('refresh').addEventListener('click', () => refreshAll().catch(err => setStatus('claim-status', err.message, 'err')));
$('logout').addEventListener('click', async () => {{ await api('/api/logout', {{ method: 'POST', body: '{{}}' }}); state.user = null; state.csrfToken = ''; renderAuth(); }});
$('account').addEventListener('keydown', event => {{ if (event.key === 'Enter') login().catch(err => setStatus('auth-status', err.message, 'err')); }});
$('claim-code').addEventListener('keydown', event => {{ if (event.key === 'Enter') claimDaemon().catch(err => setStatus('claim-status', err.message, 'err')); }});

const params = new URLSearchParams(location.search);
if (params.get('claim_code')) $('claim-code').value = params.get('claim_code');
refreshAll().catch(() => renderAuth());
</script>
</body>
</html>"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use bip39::Language;

    fn daemon_record(
        daemon_id: &str,
        owner_user_id: Option<Uuid>,
        claim_code: Option<&str>,
        claim_code_created_unix_ms: Option<u64>,
    ) -> DaemonRecord {
        DaemonRecord {
            daemon_id: daemon_id.to_string(),
            label: None,
            daemon_public_key: format!("{daemon_id}-key"),
            owner_user_id,
            claim_code_hash: claim_code.map(claim_code_hash),
            claim_code_created_unix_ms,
            registered_unix_ms: 1,
            last_seen_unix_ms: 1,
            updated_unix_ms: 1,
        }
    }

    #[test]
    fn generated_claim_code_is_12_word_bip39_mnemonic() {
        let code = generate_claim_code().unwrap();
        let parts: Vec<_> = code.split('-').collect();
        let words = Language::English.word_list();
        assert_eq!(parts.len(), 12);
        for part in &parts {
            assert!(words.contains(part), "unexpected claim word {part}");
        }
        assert_eq!(normalize_claim_code(&code), code);
        let mnemonic = Mnemonic::parse_in_normalized(Language::English, &code.replace('-', " "))
            .expect("generated phrase must be a valid BIP39 mnemonic");
        assert_eq!(mnemonic.to_entropy().len(), CLAIM_CODE_ENTROPY_BYTES);
    }

    #[test]
    fn claim_code_normalization_accepts_case_and_separator_variants() {
        let code = "abandon-ability-able-about-above-absent-absorb";
        assert_eq!(
            normalize_claim_code("  Abandon Ability--ABLE_about.above absent absorb  "),
            code
        );
        assert_eq!(claim_code_hash(code), claim_code_hash(&code.to_uppercase()));
        assert_eq!(
            claim_code_hash(code),
            claim_code_hash("abandon ability able about above absent absorb")
        );
    }

    #[test]
    fn app_route_requires_connect_mode_and_daemon_id() {
        assert!(valid_connect_app_query(Some(
            "connect=1&daemon_id=vortex-deb-x11-intendant"
        )));
        assert!(valid_connect_app_query(Some(
            "daemon_id=vortex-deb-x11-intendant&connect=1"
        )));
        assert!(!valid_connect_app_query(None));
        assert!(!valid_connect_app_query(Some("")));
        assert!(!valid_connect_app_query(Some(
            "daemon_id=vortex-deb-x11-intendant"
        )));
        assert!(!valid_connect_app_query(Some("connect=1")));
        assert!(!valid_connect_app_query(Some("connect=0&daemon_id=daemon")));
        assert!(!valid_connect_app_query(Some("connect=1&daemon_id=%20")));
    }

    #[test]
    fn access_ui_uses_access_branding() {
        let html = connect_ui_html("https://intendant.dev", "Intendant Access", "Fleet access");
        assert!(html.contains("<title>Intendant Access</title>"));
        assert!(html.contains("<h1>Intendant Access</h1>"));
        assert!(html.contains(">Fleet access</div>"));
    }

    #[test]
    fn active_claim_code_hashes_only_tracks_fresh_unclaimed_other_daemons() {
        let now = now_unix_ms();
        let fresh = "abandon-ability-able-about-above-absent-absorb";
        let current = "abstract-absurd-abuse-access-accident-account-accuse";
        let expired = "achieve-acid-acoustic-acquire-across-act-action";
        let claimed = "actor-actress-actual-adapt-add-addict-address";
        let store = Store {
            users: Vec::new(),
            daemons: vec![
                daemon_record("fresh", None, Some(fresh), Some(now)),
                daemon_record("current", None, Some(current), Some(now)),
                daemon_record(
                    "expired",
                    None,
                    Some(expired),
                    Some(now.saturating_sub(CLAIM_CODE_TTL_MS + 1)),
                ),
                daemon_record("claimed", Some(Uuid::new_v4()), Some(claimed), Some(now)),
            ],
            fleet_targets: Vec::new(),
            audit: Vec::new(),
        };
        let hashes = active_claim_code_hashes(&store, "current", now);
        assert!(hashes.contains(&claim_code_hash(fresh)));
        assert!(!hashes.contains(&claim_code_hash(current)));
        assert!(!hashes.contains(&claim_code_hash(expired)));
        assert!(!hashes.contains(&claim_code_hash(claimed)));
    }

    #[test]
    fn ensure_claim_code_reuses_fresh_unique_in_memory_code() {
        let now = now_unix_ms();
        let code = "abandon-ability-able-about-above-absent-absorb";
        let mut daemon = daemon_record("daemon", None, Some(code), Some(now));
        let mut claim_codes = HashMap::from([(daemon.daemon_id.clone(), code.to_string())]);
        let active_hashes = HashSet::new();

        let returned = ensure_claim_code(&mut claim_codes, &mut daemon, &active_hashes).unwrap();

        assert_eq!(returned, code);
        let expected_hash = claim_code_hash(code);
        assert_eq!(
            daemon.claim_code_hash.as_deref(),
            Some(expected_hash.as_str())
        );
    }

    #[test]
    fn ensure_claim_code_replaces_active_hash_collision() {
        let now = now_unix_ms();
        let code = "abandon-ability-able-about-above-absent-absorb";
        let mut daemon = daemon_record("daemon", None, Some(code), Some(now));
        let mut claim_codes = HashMap::from([(daemon.daemon_id.clone(), code.to_string())]);
        let active_hashes = HashSet::from([claim_code_hash(code)]);

        let returned = ensure_claim_code(&mut claim_codes, &mut daemon, &active_hashes).unwrap();

        assert_ne!(returned, code);
        assert!(!active_hashes.contains(&claim_code_hash(&returned)));
        let expected_hash = claim_code_hash(&returned);
        assert_eq!(
            daemon.claim_code_hash.as_deref(),
            Some(expected_hash.as_str())
        );
    }

    #[test]
    fn fleet_target_input_is_sanitized_and_capped() {
        let user_id = Uuid::new_v4();
        let now = now_unix_ms();
        let target = normalize_fleet_target_input(
            user_id,
            FleetTargetInput {
                id: " target\nid ".to_string(),
                host_id: " target\nid ".to_string(),
                label: " My target ".to_string(),
                local: true,
                source: "browser fleet!".to_string(),
                access_domain: "user_client".to_string(),
                access_domain_label: " User/client ".to_string(),
                route: "hosted_connect".to_string(),
                route_key: String::new(),
                route_label: " Hosted Connect ".to_string(),
                auth: "connect_account".to_string(),
                auth_label: " Connect account ".to_string(),
                effective_role: "root".to_string(),
                effective_role_label: " Root ".to_string(),
                profile: "root".to_string(),
                url: "javascript:alert(1)".to_string(),
                ws_url: "wss://example.test/ws".to_string(),
                browser_tcp_via_url: "/app?connect=1&daemon_id=daemon".to_string(),
                origin: "https://intendant.dev".to_string(),
                connect_daemon_id: " daemon ".to_string(),
                capabilities: vec![
                    json!("display"),
                    json!("display"),
                    json!("custom:files"),
                    json!(42),
                ],
                first_seen_unix_ms: now.saturating_add(10_000),
                last_seen_unix_ms: now.saturating_add(10_000),
            },
            now,
        )
        .expect("target should normalize");

        assert_eq!(target.user_id, user_id);
        assert_eq!(target.host_id, "targetid");
        assert_eq!(target.label, "My target");
        assert_eq!(target.source, "browserfleet");
        assert_eq!(target.url, "");
        assert_eq!(target.ws_url, "wss://example.test/ws");
        assert_eq!(
            target.browser_tcp_via_url,
            "/app?connect=1&daemon_id=daemon"
        );
        assert_eq!(target.origin, "https://intendant.dev");
        assert_eq!(target.connect_daemon_id.as_deref(), Some("daemon"));
        assert_eq!(target.capabilities, vec!["display", "custom:files"]);
        assert_eq!(target.first_seen_unix_ms, now);
        assert_eq!(target.last_seen_unix_ms, now);
    }

    #[test]
    fn fleet_targets_merge_claimed_daemons_over_remembered_records() {
        let user_id = Uuid::new_v4();
        let store = Store {
            users: Vec::new(),
            daemons: vec![DaemonRecord {
                daemon_id: "daemon-1".to_string(),
                label: Some("Live daemon".to_string()),
                daemon_public_key: "daemon-key".to_string(),
                owner_user_id: Some(user_id),
                claim_code_hash: None,
                claim_code_created_unix_ms: None,
                registered_unix_ms: 10,
                last_seen_unix_ms: now_unix_ms(),
                updated_unix_ms: 20,
            }],
            fleet_targets: vec![
                FleetTargetRecord {
                    user_id,
                    id: "daemon-1".to_string(),
                    host_id: "daemon-1".to_string(),
                    label: "Stale label".to_string(),
                    local: false,
                    source: "browser_fleet".to_string(),
                    access_domain: "user_client".to_string(),
                    access_domain_label: "User/client access".to_string(),
                    route: "hosted_connect".to_string(),
                    route_label: "Hosted Connect".to_string(),
                    auth: "connect_account".to_string(),
                    auth_label: "Connect account".to_string(),
                    effective_role: "root".to_string(),
                    effective_role_label: "Root".to_string(),
                    profile: String::new(),
                    url: "/app?connect=1&daemon_id=daemon-1".to_string(),
                    ws_url: String::new(),
                    browser_tcp_via_url: String::new(),
                    origin: "https://intendant.dev".to_string(),
                    connect_daemon_id: Some("daemon-1".to_string()),
                    capabilities: Vec::new(),
                    first_seen_unix_ms: 1,
                    last_seen_unix_ms: 1,
                    updated_unix_ms: 1,
                },
                FleetTargetRecord {
                    user_id,
                    id: "intendant:192.168.64.61".to_string(),
                    host_id: "intendant:192.168.64.61".to_string(),
                    label: "192.168.64.61".to_string(),
                    local: true,
                    source: "dashboard".to_string(),
                    access_domain: "user_client".to_string(),
                    access_domain_label: "User/client access".to_string(),
                    route: "current_dashboard".to_string(),
                    route_label: "Current dashboard".to_string(),
                    auth: "trusted_dashboard".to_string(),
                    auth_label: "Trusted dashboard session".to_string(),
                    effective_role: "root".to_string(),
                    effective_role_label: "Root".to_string(),
                    profile: String::new(),
                    url: "/app?connect=1&daemon_id=daemon-1".to_string(),
                    ws_url: String::new(),
                    browser_tcp_via_url: String::new(),
                    origin: "https://connect.intendant.dev".to_string(),
                    connect_daemon_id: Some("daemon-1".to_string()),
                    capabilities: Vec::new(),
                    first_seen_unix_ms: 1,
                    last_seen_unix_ms: 1,
                    updated_unix_ms: 1,
                },
                FleetTargetRecord {
                    user_id,
                    id: "manual".to_string(),
                    host_id: "manual".to_string(),
                    label: "Manual target".to_string(),
                    local: false,
                    source: "browser_fleet".to_string(),
                    access_domain: String::new(),
                    access_domain_label: String::new(),
                    route: String::new(),
                    route_label: "Remembered route".to_string(),
                    auth: String::new(),
                    auth_label: String::new(),
                    effective_role: String::new(),
                    effective_role_label: String::new(),
                    profile: String::new(),
                    url: "https://manual.example".to_string(),
                    ws_url: String::new(),
                    browser_tcp_via_url: String::new(),
                    origin: "https://intendant.dev".to_string(),
                    connect_daemon_id: None,
                    capabilities: Vec::new(),
                    first_seen_unix_ms: 1,
                    last_seen_unix_ms: 1,
                    updated_unix_ms: 1,
                },
            ],
            audit: Vec::new(),
        };
        let config = ServiceConfig {
            listen: SocketAddr::from(([127, 0, 0, 1], 9876)),
            public_origin: "https://intendant.dev".to_string(),
            rp_id: "intendant.dev".to_string(),
            static_root: PathBuf::from("static"),
            data_file: PathBuf::from("state.json"),
            daemon_token: None,
            cookie_secure: true,
        };

        let targets = fleet_targets_for_user(&config, &store, user_id);
        assert_eq!(targets.len(), 2);
        let live = targets
            .iter()
            .find(|target| target.get("host_id").and_then(|v| v.as_str()) == Some("daemon-1"))
            .expect("live daemon target");
        assert_eq!(
            live.get("label").and_then(|v| v.as_str()),
            Some("Live daemon")
        );
        assert_eq!(
            live.get("source").and_then(|v| v.as_str()),
            Some("connect_daemon")
        );
        let manual = targets
            .iter()
            .find(|target| target.get("host_id").and_then(|v| v.as_str()) == Some("manual"))
            .expect("manual target");
        assert_eq!(
            manual.get("source").and_then(|v| v.as_str()),
            Some("browser_fleet")
        );
    }
}
