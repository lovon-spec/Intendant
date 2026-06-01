use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tokio::process::Child;
use tokio::sync::RwLock;

pub type SharedBrowserWorkspaceRegistry = Arc<RwLock<BrowserWorkspaceRegistry>>;

static GLOBAL_BROWSER_WORKSPACES: OnceLock<SharedBrowserWorkspaceRegistry> = OnceLock::new();

const CDP_STARTUP_TIMEOUT: Duration = Duration::from_secs(8);

pub fn global_registry() -> SharedBrowserWorkspaceRegistry {
    GLOBAL_BROWSER_WORKSPACES
        .get_or_init(|| Arc::new(RwLock::new(BrowserWorkspaceRegistry::default())))
        .clone()
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BrowserWorkspaceProvider {
    Auto,
    Cdp,
    Playwright,
    AgentBrowser,
    Stream,
}

impl BrowserWorkspaceProvider {
    pub fn parse(raw: Option<&str>) -> Self {
        match raw
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("auto")
            .to_ascii_lowercase()
            .as_str()
        {
            "cdp" | "chrome" | "chromium" => Self::Cdp,
            "playwright" => Self::Playwright,
            "agent_browser" | "agent-browser" | "agentbrowser" => Self::AgentBrowser,
            "stream" | "streamed" | "remote_stream" | "remote-stream" => Self::Stream,
            _ => Self::Auto,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Cdp => "cdp",
            Self::Playwright => "playwright",
            Self::AgentBrowser => "agent_browser",
            Self::Stream => "stream",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BrowserWorkspaceStatus {
    Starting,
    Ready,
    Closed,
    Error,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BrowserWorkspacePreviewMode {
    Semantic,
    Screenshot,
    Stream,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BrowserWorkspacePlacement {
    /// "local" or "peer". Kept stringly on the wire so older clients can
    /// forward unknown future placement kinds.
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peer_id: Option<String>,
}

impl BrowserWorkspacePlacement {
    pub fn local() -> Self {
        Self {
            kind: "local".to_string(),
            peer_id: None,
        }
    }

    pub fn peer(peer_id: String) -> Self {
        Self {
            kind: "peer".to_string(),
            peer_id: Some(peer_id),
        }
    }

    pub fn is_local(&self) -> bool {
        self.kind.eq_ignore_ascii_case("local") && self.peer_id.is_none()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BrowserWorkspaceLease {
    pub holder_id: String,
    pub holder_kind: String,
    pub acquired_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BrowserWorkspace {
    pub id: String,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    pub provider: BrowserWorkspaceProvider,
    pub requested_provider: BrowserWorkspaceProvider,
    pub placement: BrowserWorkspacePlacement,
    pub status: BrowserWorkspaceStatus,
    pub preview_mode: BrowserWorkspacePreviewMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile_dir: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub browser_executable: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process_id: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub debugging_port: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cdp_http_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cdp_ws_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_target_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lease: Option<BrowserWorkspaceLease>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BrowserProviderStatus {
    pub provider: BrowserWorkspaceProvider,
    pub available: bool,
    pub executable: Option<String>,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateBrowserWorkspaceRequest {
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default)]
    pub peer_id: Option<String>,
    #[serde(default)]
    pub owner_session_id: Option<String>,
    #[serde(default)]
    pub profile_dir: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AcquireBrowserWorkspaceRequest {
    pub workspace_id: String,
    pub holder_id: String,
    #[serde(default)]
    pub holder_kind: Option<String>,
    #[serde(default)]
    pub note: Option<String>,
    #[serde(default)]
    pub force: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReleaseBrowserWorkspaceRequest {
    pub workspace_id: String,
    #[serde(default)]
    pub holder_id: Option<String>,
    #[serde(default)]
    pub note: Option<String>,
}

#[derive(Debug)]
pub enum BrowserWorkspaceError {
    NotFound(String),
    LeaseHeld {
        workspace_id: String,
        holder_id: String,
    },
    Unsupported(String),
    Io(String),
    Launch(String),
}

impl fmt::Display for BrowserWorkspaceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound(id) => write!(f, "browser workspace '{id}' not found"),
            Self::LeaseHeld {
                workspace_id,
                holder_id,
            } => write!(
                f,
                "browser workspace '{workspace_id}' is already leased by '{holder_id}'"
            ),
            Self::Unsupported(msg) | Self::Io(msg) | Self::Launch(msg) => f.write_str(msg),
        }
    }
}

impl std::error::Error for BrowserWorkspaceError {}

#[derive(Default)]
pub struct BrowserWorkspaceRegistry {
    workspaces: BTreeMap<String, BrowserWorkspace>,
    children: HashMap<String, Child>,
}

impl BrowserWorkspaceRegistry {
    pub fn list(&self) -> Vec<BrowserWorkspace> {
        self.workspaces.values().cloned().collect()
    }

    fn insert(&mut self, workspace: BrowserWorkspace, child: Option<Child>) {
        if let Some(child) = child {
            self.children.insert(workspace.id.clone(), child);
        }
        self.workspaces.insert(workspace.id.clone(), workspace);
    }

    fn remove(&mut self, id: &str) -> Option<(BrowserWorkspace, Option<Child>)> {
        let workspace = self.workspaces.remove(id)?;
        let child = self.children.remove(id);
        Some((workspace, child))
    }

    fn acquire(
        &mut self,
        request: AcquireBrowserWorkspaceRequest,
    ) -> Result<BrowserWorkspace, BrowserWorkspaceError> {
        let workspace = self
            .workspaces
            .get_mut(&request.workspace_id)
            .ok_or_else(|| BrowserWorkspaceError::NotFound(request.workspace_id.clone()))?;
        if let Some(lease) = workspace.lease.as_ref() {
            if lease.holder_id != request.holder_id && !request.force {
                return Err(BrowserWorkspaceError::LeaseHeld {
                    workspace_id: request.workspace_id,
                    holder_id: lease.holder_id.clone(),
                });
            }
        }
        workspace.lease = Some(BrowserWorkspaceLease {
            holder_id: request.holder_id,
            holder_kind: request
                .holder_kind
                .unwrap_or_else(|| "agent".to_string())
                .trim()
                .to_string(),
            acquired_at: now_string(),
            note: request.note,
        });
        workspace.updated_at = now_string();
        Ok(workspace.clone())
    }

    fn release(
        &mut self,
        request: ReleaseBrowserWorkspaceRequest,
    ) -> Result<BrowserWorkspace, BrowserWorkspaceError> {
        let workspace = self
            .workspaces
            .get_mut(&request.workspace_id)
            .ok_or_else(|| BrowserWorkspaceError::NotFound(request.workspace_id.clone()))?;
        if let (Some(expected), Some(lease)) =
            (request.holder_id.as_deref(), workspace.lease.as_ref())
        {
            if !expected.trim().is_empty() && lease.holder_id != expected {
                return Err(BrowserWorkspaceError::LeaseHeld {
                    workspace_id: request.workspace_id,
                    holder_id: lease.holder_id.clone(),
                });
            }
        }
        workspace.lease = None;
        if let Some(note) = request.note.filter(|s| !s.trim().is_empty()) {
            workspace.message = Some(note);
        }
        workspace.updated_at = now_string();
        Ok(workspace.clone())
    }
}

pub async fn provider_statuses() -> Vec<BrowserProviderStatus> {
    let cdp_exe = find_chromium_executable();
    vec![
        BrowserProviderStatus {
            provider: BrowserWorkspaceProvider::Cdp,
            available: cdp_exe.is_some(),
            executable: cdp_exe.map(|p| p.display().to_string()),
            message: "Local Chrome/Chromium through the Chrome DevTools Protocol.".to_string(),
        },
        BrowserProviderStatus {
            provider: BrowserWorkspaceProvider::Playwright,
            available: find_executable("playwright")
                .or_else(|| find_executable("npx"))
                .is_some(),
            executable: find_executable("playwright")
                .or_else(|| find_executable("npx"))
                .map(|p| p.display().to_string()),
            message: "Provider contract reserved for the Playwright sidecar.".to_string(),
        },
        BrowserProviderStatus {
            provider: BrowserWorkspaceProvider::AgentBrowser,
            available: find_executable("agent-browser").is_some(),
            executable: find_executable("agent-browser").map(|p| p.display().to_string()),
            message: "Provider contract reserved for Vercel Agent Browser integration.".to_string(),
        },
        BrowserProviderStatus {
            provider: BrowserWorkspaceProvider::Stream,
            available: true,
            executable: None,
            message:
                "Fallback to Intendant display streaming for remote or non-browser workspaces."
                    .to_string(),
        },
    ]
}

pub async fn create_workspace(
    request: CreateBrowserWorkspaceRequest,
) -> Result<BrowserWorkspace, BrowserWorkspaceError> {
    let requested_provider = BrowserWorkspaceProvider::parse(request.provider.as_deref());
    let placement = match request
        .peer_id
        .as_ref()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
    {
        Some(peer_id) => BrowserWorkspacePlacement::peer(peer_id.to_string()),
        None => BrowserWorkspacePlacement::local(),
    };
    if !placement.is_local() {
        return Err(BrowserWorkspaceError::Unsupported(
            "remote peer browser workspace placement is modeled but not wired to the federation transport yet"
                .to_string(),
        ));
    }

    let provider = match requested_provider {
        BrowserWorkspaceProvider::Auto => BrowserWorkspaceProvider::Cdp,
        BrowserWorkspaceProvider::Cdp => BrowserWorkspaceProvider::Cdp,
        BrowserWorkspaceProvider::Playwright => {
            return Err(BrowserWorkspaceError::Unsupported(
                "Playwright browser workspaces need the sidecar driver; use provider=cdp for the first executable backend"
                    .to_string(),
            ));
        }
        BrowserWorkspaceProvider::AgentBrowser => {
            return Err(BrowserWorkspaceError::Unsupported(
                "Agent Browser workspaces need the Agent Browser provider adapter; use provider=cdp for the first executable backend"
                    .to_string(),
            ));
        }
        BrowserWorkspaceProvider::Stream => {
            return Err(BrowserWorkspaceError::Unsupported(
                "stream workspaces are represented by the existing display/shared-view path; create a display stream instead"
                    .to_string(),
            ));
        }
    };

    let id = format!("bw-{}", uuid::Uuid::new_v4().simple());
    let created_at = now_string();
    let profile_dir = request
        .profile_dir
        .as_deref()
        .map(PathBuf::from)
        .unwrap_or_else(|| default_profile_dir(&id));
    std::fs::create_dir_all(&profile_dir).map_err(|e| {
        BrowserWorkspaceError::Io(format!(
            "failed to create browser workspace profile {}: {e}",
            profile_dir.display()
        ))
    })?;

    let mut workspace = BrowserWorkspace {
        label: request
            .label
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("Browser workspace")
            .to_string(),
        url: request
            .url
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string),
        provider,
        requested_provider,
        placement,
        status: BrowserWorkspaceStatus::Starting,
        preview_mode: BrowserWorkspacePreviewMode::Semantic,
        owner_session_id: request
            .owner_session_id
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string),
        profile_dir: Some(profile_dir.display().to_string()),
        browser_executable: None,
        process_id: None,
        debugging_port: None,
        cdp_http_url: None,
        cdp_ws_url: None,
        active_target_id: None,
        lease: None,
        message: Some("starting local CDP browser".to_string()),
        created_at: created_at.clone(),
        updated_at: created_at,
        id,
    };

    let (child, cdp) = launch_cdp_browser(&workspace, &profile_dir).await?;
    workspace.browser_executable = Some(cdp.executable.display().to_string());
    workspace.process_id = cdp.process_id;
    workspace.debugging_port = Some(cdp.port);
    workspace.cdp_http_url = Some(format!("http://127.0.0.1:{}", cdp.port));
    workspace.cdp_ws_url = cdp.web_socket_debugger_url;
    workspace.active_target_id = cdp.target_id;
    workspace.status = BrowserWorkspaceStatus::Ready;
    workspace.message = Some("ready".to_string());
    workspace.updated_at = now_string();

    global_registry()
        .write()
        .await
        .insert(workspace.clone(), Some(child));
    Ok(workspace)
}

pub async fn list_workspaces() -> Vec<BrowserWorkspace> {
    global_registry().read().await.list()
}

pub async fn close_workspace(
    id: &str,
    reason: Option<String>,
) -> Result<BrowserWorkspace, BrowserWorkspaceError> {
    let (mut workspace, mut child) = global_registry()
        .write()
        .await
        .remove(id)
        .ok_or_else(|| BrowserWorkspaceError::NotFound(id.to_string()))?;
    workspace.status = BrowserWorkspaceStatus::Closed;
    workspace.lease = None;
    workspace.message = reason.or_else(|| Some("closed".to_string()));
    workspace.updated_at = now_string();
    if let Some(pid) = workspace.process_id {
        let _ = crate::platform::terminate_process_tree_now(pid);
    }
    if let Some(child) = child.as_mut() {
        let _ = child.start_kill();
    }
    Ok(workspace)
}

pub async fn acquire_workspace(
    request: AcquireBrowserWorkspaceRequest,
) -> Result<BrowserWorkspace, BrowserWorkspaceError> {
    global_registry().write().await.acquire(request)
}

pub async fn release_workspace(
    request: ReleaseBrowserWorkspaceRequest,
) -> Result<BrowserWorkspace, BrowserWorkspaceError> {
    global_registry().write().await.release(request)
}

struct CdpLaunch {
    executable: PathBuf,
    process_id: Option<u32>,
    port: u16,
    web_socket_debugger_url: Option<String>,
    target_id: Option<String>,
}

async fn launch_cdp_browser(
    workspace: &BrowserWorkspace,
    profile_dir: &Path,
) -> Result<(Child, CdpLaunch), BrowserWorkspaceError> {
    let executable = find_chromium_executable().ok_or_else(|| {
        BrowserWorkspaceError::Launch(
            "no Chrome/Chromium executable found for CDP browser workspace".to_string(),
        )
    })?;
    let port = reserve_local_port().await?;
    let mut command = tokio::process::Command::new(&executable);
    command
        .arg(format!("--remote-debugging-port={port}"))
        .arg("--remote-debugging-address=127.0.0.1")
        .arg(format!("--user-data-dir={}", profile_dir.display()))
        .arg("--no-first-run")
        .arg("--no-default-browser-check")
        .arg("--disable-background-networking")
        .arg("--disable-features=Translate");
    if let Some(url) = workspace.url.as_ref() {
        command.arg(url);
    } else {
        command.arg("about:blank");
    }
    let child = command.spawn().map_err(|e| {
        BrowserWorkspaceError::Launch(format!("failed to launch {}: {e}", executable.display()))
    })?;
    let process_id = child.id();
    match wait_for_cdp_target(port).await {
        Ok((ws, target_id)) => Ok((
            child,
            CdpLaunch {
                executable,
                process_id,
                port,
                web_socket_debugger_url: ws,
                target_id,
            },
        )),
        Err(err) => {
            if let Some(pid) = process_id {
                let _ = crate::platform::terminate_process_tree_now(pid);
            }
            Err(err)
        }
    }
}

async fn reserve_local_port() -> Result<u16, BrowserWorkspaceError> {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .map_err(|e| BrowserWorkspaceError::Io(format!("failed to reserve CDP port: {e}")))?;
    let port = listener
        .local_addr()
        .map_err(|e| BrowserWorkspaceError::Io(format!("failed to read CDP port: {e}")))?
        .port();
    drop(listener);
    Ok(port)
}

async fn wait_for_cdp_target(
    port: u16,
) -> Result<(Option<String>, Option<String>), BrowserWorkspaceError> {
    let client = reqwest::Client::new();
    let deadline = tokio::time::Instant::now() + CDP_STARTUP_TIMEOUT;
    let list_url = format!("http://127.0.0.1:{port}/json/list");
    loop {
        match client.get(&list_url).send().await {
            Ok(resp) if resp.status().is_success() => {
                let targets: serde_json::Value = resp.json().await.map_err(|e| {
                    BrowserWorkspaceError::Launch(format!(
                        "failed to parse CDP target list from {list_url}: {e}"
                    ))
                })?;
                if let Some((ws, id)) = first_page_target(&targets) {
                    return Ok((ws, id));
                }
            }
            _ => {}
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(BrowserWorkspaceError::Launch(format!(
                "timed out waiting for CDP target at {list_url}"
            )));
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
}

fn first_page_target(value: &serde_json::Value) -> Option<(Option<String>, Option<String>)> {
    let targets = value.as_array()?;
    targets
        .iter()
        .find(|target| target.get("type").and_then(|v| v.as_str()) == Some("page"))
        .map(|target| {
            (
                target
                    .get("webSocketDebuggerUrl")
                    .and_then(|v| v.as_str())
                    .map(str::to_string),
                target
                    .get("id")
                    .and_then(|v| v.as_str())
                    .map(str::to_string),
            )
        })
}

fn find_chromium_executable() -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        for path in [
            "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
            "/Applications/Chromium.app/Contents/MacOS/Chromium",
            "/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge",
            "/Applications/Brave Browser.app/Contents/MacOS/Brave Browser",
        ] {
            let p = PathBuf::from(path);
            if p.exists() {
                return Some(p);
            }
        }
    }
    for name in [
        "google-chrome",
        "chrome",
        "chromium",
        "chromium-browser",
        "msedge",
        "brave-browser",
    ] {
        if let Some(path) = find_executable(name) {
            return Some(path);
        }
    }
    None
}

fn find_executable(name: &str) -> Option<PathBuf> {
    which::which(name).ok()
}

fn default_profile_dir(id: &str) -> PathBuf {
    let base = dirs::data_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("intendant")
        .join("browser-workspaces");
    base.join(id).join("profile")
}

fn now_string() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_workspace(id: &str) -> BrowserWorkspace {
        BrowserWorkspace {
            id: id.to_string(),
            label: "Test".to_string(),
            url: Some("http://localhost:8765".to_string()),
            provider: BrowserWorkspaceProvider::Cdp,
            requested_provider: BrowserWorkspaceProvider::Auto,
            placement: BrowserWorkspacePlacement::local(),
            status: BrowserWorkspaceStatus::Ready,
            preview_mode: BrowserWorkspacePreviewMode::Semantic,
            owner_session_id: Some("session-1".to_string()),
            profile_dir: None,
            browser_executable: None,
            process_id: None,
            debugging_port: None,
            cdp_http_url: None,
            cdp_ws_url: None,
            active_target_id: None,
            lease: None,
            message: None,
            created_at: "2026-05-31T00:00:00.000Z".to_string(),
            updated_at: "2026-05-31T00:00:00.000Z".to_string(),
        }
    }

    #[test]
    fn lease_blocks_second_holder_without_force() {
        let mut registry = BrowserWorkspaceRegistry::default();
        registry.insert(sample_workspace("bw-test"), None);
        let first = registry
            .acquire(AcquireBrowserWorkspaceRequest {
                workspace_id: "bw-test".to_string(),
                holder_id: "agent-a".to_string(),
                holder_kind: Some("agent".to_string()),
                note: None,
                force: false,
            })
            .unwrap();
        assert_eq!(first.lease.unwrap().holder_id, "agent-a");

        let second = registry.acquire(AcquireBrowserWorkspaceRequest {
            workspace_id: "bw-test".to_string(),
            holder_id: "agent-b".to_string(),
            holder_kind: Some("agent".to_string()),
            note: None,
            force: false,
        });
        assert!(matches!(
            second,
            Err(BrowserWorkspaceError::LeaseHeld { .. })
        ));
    }

    #[test]
    fn force_acquire_replaces_holder() {
        let mut registry = BrowserWorkspaceRegistry::default();
        registry.insert(sample_workspace("bw-test"), None);
        registry
            .acquire(AcquireBrowserWorkspaceRequest {
                workspace_id: "bw-test".to_string(),
                holder_id: "agent-a".to_string(),
                holder_kind: Some("agent".to_string()),
                note: None,
                force: false,
            })
            .unwrap();
        let forced = registry
            .acquire(AcquireBrowserWorkspaceRequest {
                workspace_id: "bw-test".to_string(),
                holder_id: "agent-b".to_string(),
                holder_kind: Some("agent".to_string()),
                note: Some("takeover".to_string()),
                force: true,
            })
            .unwrap();
        assert_eq!(forced.lease.unwrap().holder_id, "agent-b");
    }

    #[test]
    fn cdp_target_parser_prefers_page() {
        let targets = serde_json::json!([
            {"type":"service_worker","id":"worker"},
            {"type":"page","id":"page-1","webSocketDebuggerUrl":"ws://127.0.0.1/devtools/page/page-1"}
        ]);
        let (ws, id) = first_page_target(&targets).unwrap();
        assert_eq!(id.as_deref(), Some("page-1"));
        assert_eq!(ws.as_deref(), Some("ws://127.0.0.1/devtools/page/page-1"));
    }
}
