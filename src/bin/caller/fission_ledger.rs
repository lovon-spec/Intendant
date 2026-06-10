use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, OnceLock};

/// The wire/back-compat fission ledger view: just the groups, no extension
/// state. Read by the MCP `get_status` surface and embedded into rewind-record
/// snapshots (`context_rewind.rs`); see [`FissionLedgerDocument`] for the full
/// on-disk shape.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct FissionLedger {
    pub groups: Vec<FissionGroup>,
}

/// One fission group: every branch spawned at a single `(parent session,
/// anchor item)` tool call, plus the optional canonical-continuation claim.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FissionGroup {
    pub group_id: String,
    pub parent_session_id: String,
    pub anchor_item_id: String,
    pub tool: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub objective: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub canonical_session_id: Option<String>,
    pub branches: Vec<FissionBranch>,
}

/// One spawned sibling/fork within a [`FissionGroup`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FissionBranch {
    pub session_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backend_session_id: Option<String>,
    /// Branch lifecycle status. Canonical vocabulary:
    /// `running | blocked | completed | failed | detached | cancelled`
    /// (legacy/observed values such as `ended`, `interrupted`, and `unknown`
    /// also occur — see [`normalize_branch_status`]). Kept as a free string
    /// for wire/back compat with ledgers written before the vocabulary
    /// existed. `detached` and `cancelled` are *sticky*: only an explicit API
    /// (the detach functions, a future cancel/reattach API) may change them.
    /// [`record_fission_observation`] never does, so a stray completion event
    /// from a still-running child of a detached anchor cannot resurrect the
    /// branch. See [`branch_status_is_terminal`] / [`branch_status_is_sticky`].
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worktree_path: Option<PathBuf>,
    pub raw_log: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub ephemeral: bool,
    pub updated_at: String,
}

/// The full on-disk fission ledger document: the wire/back-compat
/// [`FissionLedger`] groups plus the [`FissionLedgerExt`] extension state
/// (detach markers, import markers, charters, work metadata).
///
/// `FissionLedger`/`FissionGroup`/`FissionBranch` are constructed as full
/// struct literals elsewhere in the codebase (rewind-record snapshots and
/// their tests), so their field sets are frozen; everything the fission flow
/// needs beyond them lives in `ext`, inside the same atomically-written
/// `fission_ledger.json`. Old ledger files (no `ext` key) deserialize with an
/// empty extension, and old readers deserializing a new file simply ignore
/// `ext` — and while `ext` is empty the file stays byte-identical to the old
/// format.
///
/// Read-side callers that need detach/import/charter state (dashboard wiring,
/// MCP status tools) use [`read_fission_ledger_document`] /
/// [`read_fission_ledger_document_for_session`] instead of the plain ledger
/// readers.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct FissionLedgerDocument {
    #[serde(default)]
    pub groups: Vec<FissionGroup>,
    #[serde(default, skip_serializing_if = "FissionLedgerExt::is_empty")]
    pub ext: FissionLedgerExt,
}

impl FissionLedgerDocument {
    /// Split off the wire/back-compat ledger view (drops extension state).
    /// For callers that feed a plain [`FissionLedger`] into existing surfaces
    /// (e.g. rewind-record snapshots).
    #[allow(dead_code)] // wired by the fission/lineage integration stage
    pub fn into_ledger(self) -> FissionLedger {
        FissionLedger {
            groups: self.groups,
        }
    }

    /// Extension entry for a group, if any state was recorded for it.
    #[allow(dead_code)] // wired by the fission/lineage integration stage
    pub fn group_ext(&self, group_id: &str) -> Option<&FissionGroupExt> {
        self.ext.group(group_id)
    }

    /// Extension entry for a branch, if any state was recorded for it.
    #[allow(dead_code)] // wired by the fission/lineage integration stage
    pub fn branch_ext(&self, group_id: &str, branch_session_id: &str) -> Option<&FissionBranchExt> {
        self.ext.group(group_id)?.branch(branch_session_id)
    }

    /// True when the group has been detached (its anchor left the effective
    /// history, or it was explicitly severed).
    #[allow(dead_code)] // wired by the fission/lineage integration stage
    pub fn group_is_detached(&self, group_id: &str) -> bool {
        self.ext
            .group(group_id)
            .is_some_and(FissionGroupExt::is_detached)
    }
}

/// Extension state for the fission ledger (see [`FissionLedgerDocument`]).
/// Entries are keyed by group/branch ids and are created lazily, only when
/// something is recorded.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct FissionLedgerExt {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub groups: Vec<FissionGroupExt>,
}

impl FissionLedgerExt {
    /// True when there is no extension state at all; used by serde so files
    /// without extension state stay byte-identical to the old format.
    pub fn is_empty(&self) -> bool {
        self.groups.is_empty()
    }

    /// Extension entry for a group, if any state was recorded for it.
    pub fn group(&self, group_id: &str) -> Option<&FissionGroupExt> {
        self.groups.iter().find(|group| group.group_id == group_id)
    }

    fn group_mut_or_insert(&mut self, group_id: &str) -> &mut FissionGroupExt {
        if let Some(idx) = self
            .groups
            .iter()
            .position(|group| group.group_id == group_id)
        {
            return &mut self.groups[idx];
        }
        self.groups.push(FissionGroupExt {
            group_id: group_id.to_string(),
            ..FissionGroupExt::default()
        });
        self.groups.last_mut().expect("pushed group ext")
    }
}

/// Per-group extension state, keyed by [`FissionGroup::group_id`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct FissionGroupExt {
    pub group_id: String,
    /// RFC 3339 time the group was detached. Set once by
    /// [`detach_groups_with_invalid_anchors`] / [`detach_group`] and then
    /// preserved (re-detaching is a no-op), so it records the *first* moment
    /// the anchor was severed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detached_at: Option<String>,
    /// Why the group was detached (e.g.
    /// [`DETACH_REASON_ANCHOR_UNREACHABLE`], or a caller-supplied reason).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detach_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub branches: Vec<FissionBranchExt>,
}

impl FissionGroupExt {
    /// True when the group has been detached.
    pub fn is_detached(&self) -> bool {
        self.detached_at.is_some()
    }

    /// Extension entry for a branch, if any state was recorded for it.
    pub fn branch(&self, branch_session_id: &str) -> Option<&FissionBranchExt> {
        self.branches
            .iter()
            .find(|branch| branch.session_id == branch_session_id)
    }

    fn branch_mut_or_insert(&mut self, branch_session_id: &str) -> &mut FissionBranchExt {
        if let Some(idx) = self
            .branches
            .iter()
            .position(|branch| branch.session_id == branch_session_id)
        {
            return &mut self.branches[idx];
        }
        self.branches.push(FissionBranchExt {
            session_id: branch_session_id.to_string(),
            ..FissionBranchExt::default()
        });
        self.branches.last_mut().expect("pushed branch ext")
    }
}

/// Per-branch extension state, keyed by [`FissionBranch::session_id`] within
/// the owning group.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct FissionBranchExt {
    pub session_id: String,
    /// RFC 3339 time the branch's result was last explicitly imported into
    /// the active lineage (see [`mark_branch_imported`]); re-importing
    /// refreshes the timestamp. Import never changes the branch status — a
    /// detached branch stays detached even after its artifact is salvaged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub imported_at: Option<String>,
    /// The charter the parent recorded at spawn time (model-driven fission via
    /// [`register_spawned_branch`]); observation-discovered branches have none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub charter: Option<BranchCharter>,
    /// Files the branch reported changing: a first-seen-ordered, deduplicated
    /// union across [`update_branch_work`] calls.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub changed_files: Vec<String>,
    /// Test invocations the branch reported running, merged like
    /// `changed_files`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tests_run: Vec<String>,
}

/// The scoped mandate a parent gives a model-driven fission branch at spawn
/// time. Stored per branch ([`FissionBranchExt::charter`]); supplied by the
/// spawn MCP tool that a later stage adds.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct BranchCharter {
    /// What the branch exists to accomplish.
    pub objective: String,
    /// Optional write-scope constraint (e.g. paths the branch may edit).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub write_scope: Option<String>,
    /// Whether the spawner asked for an isolated worktree.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub worktree_requested: bool,
}

/// Identity and launch metadata for a branch being registered via
/// [`register_spawned_branch`]. Construct with functional-update syntax
/// (`NewSpawnedBranch { session_id, ..Default::default() }`) so later field
/// additions don't break callers.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[allow(dead_code)] // wired by the fission/lineage integration stage
pub struct NewSpawnedBranch {
    pub session_id: String,
    pub backend_session_id: Option<String>,
    pub worktree_path: Option<PathBuf>,
    pub task: Option<String>,
    pub model: Option<String>,
    pub reasoning_effort: Option<String>,
    /// Raw-log pointer; when `None` the ledger synthesizes the
    /// `session.jsonl#session_id=…` convention used by observed branches.
    pub raw_log: Option<String>,
}

/// What a detach call changed; returned so the rewind path can log/surface the
/// exact set of severed groups, flipped branches, and voided canonical claims.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct DetachReport {
    /// Groups marked detached by this call (already-detached groups are not
    /// re-reported).
    pub detached_group_ids: Vec<String>,
    /// Branches whose status was flipped to `detached` by this call (branches
    /// already terminal kept their status and are not listed).
    pub detached_branch_session_ids: Vec<String>,
    /// Canonical claims cleared because the canonical branch was detached.
    pub cleared_canonicals: Vec<ClearedCanonical>,
}

impl DetachReport {
    /// True when the call changed nothing.
    #[allow(dead_code)] // wired by the fission/lineage integration stage
    pub fn is_empty(&self) -> bool {
        self.detached_group_ids.is_empty()
            && self.detached_branch_session_ids.is_empty()
            && self.cleared_canonicals.is_empty()
    }
}

/// A canonical claim voided by a detach: the branch that owned canonical
/// continuation was itself detached, and a detached branch cannot own
/// canonical continuation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ClearedCanonical {
    pub group_id: String,
    pub canonical_session_id: String,
}

/// A passively observed fission event (a Codex collab/sub-agent tool call and
/// its reported children), assembled by the observation hooks in `main.rs` /
/// `mcp.rs` and fed to [`record_fission_observation`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FissionObservation {
    pub parent_session_id: String,
    pub anchor_item_id: String,
    pub tool: String,
    pub status: String,
    pub prompt: Option<String>,
    pub model: Option<String>,
    pub reasoning_effort: Option<String>,
    pub branches: Vec<FissionBranchObservation>,
}

/// One child reported inside a [`FissionObservation`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FissionBranchObservation {
    pub session_id: String,
    pub status: String,
    pub summary: Option<String>,
}

/// Why a canonical-continuation claim was refused. Returned by
/// [`claim_canonical`] / [`claim_canonical_checked`]; the
/// `claim_fission_canonical` MCP tool renders it via `Display`.
#[derive(Debug)]
pub enum ClaimCanonicalError {
    Io(io::Error),
    GroupNotFound(String),
    BranchNotFound {
        group_id: String,
        branch_session_id: String,
    },
    Conflict {
        group_id: String,
        expected: Option<String>,
        current: Option<String>,
    },
    /// The group is detached, or its anchor is no longer reachable from the
    /// active lineage. Canonical continuation can only be claimed at an anchor
    /// that is still part of the effective (post-rewind) history.
    AnchorDetached {
        group_id: String,
        anchor_item_id: String,
    },
    /// The claiming branch carries a sticky `detached`/`cancelled` status and
    /// therefore cannot own canonical continuation.
    BranchDetached {
        group_id: String,
        branch_session_id: String,
        status: String,
    },
}

impl fmt::Display for ClaimCanonicalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(err) => write!(f, "{err}"),
            Self::GroupNotFound(group_id) => {
                write!(f, "fission group `{group_id}` was not found")
            }
            Self::BranchNotFound {
                group_id,
                branch_session_id,
            } => write!(
                f,
                "branch `{branch_session_id}` is not part of fission group `{group_id}`"
            ),
            Self::Conflict {
                group_id,
                expected,
                current,
            } => write!(
                f,
                "canonical claim conflict for `{group_id}`: expected {}, current {}",
                display_optional_id(expected),
                display_optional_id(current)
            ),
            Self::AnchorDetached {
                group_id,
                anchor_item_id,
            } => write!(
                f,
                "fission anchor `{anchor_item_id}` of group `{group_id}` is detached or no longer reachable from the active lineage; canonical continuation cannot be claimed at a detached anchor"
            ),
            Self::BranchDetached {
                group_id,
                branch_session_id,
                status,
            } => write!(
                f,
                "branch `{branch_session_id}` of fission group `{group_id}` is `{status}` and cannot claim canonical continuation"
            ),
        }
    }
}

impl std::error::Error for ClaimCanonicalError {}

impl From<io::Error> for ClaimCanonicalError {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}

/// Absolute path of the fission ledger file under the session log dir; shared
/// by every reader/writer in this module.
pub fn ledger_path(log_dir: &Path) -> PathBuf {
    log_dir.join("fission_ledger.json")
}

/// Stable group id for a `(parent session, spawn anchor)` pair — the keying
/// shared by [`record_fission_observation`] and [`register_spawned_branch`],
/// so observed collab events and explicit registrations for the same anchor
/// land in the same group. Also used by external callers (MCP tools) to
/// address a group.
pub fn group_id(parent_session_id: &str, anchor_item_id: &str) -> String {
    // The slugs are lossy (non-alphanumerics collapse to `_`, truncated to 96
    // chars) and are joined with `-`, which is itself a legal slug char — so
    // distinct (parent, anchor) pairs can slug to the same string. Append a
    // stable hash of the exact raw bytes so the id stays collision-resistant
    // while the slug remains human-readable.
    format!(
        "fission-{}-{}-{}",
        stable_slug(parent_session_id),
        stable_slug(anchor_item_id),
        stable_pair_hash(parent_session_id, anchor_item_id),
    )
}

/// Read the plain wire/back-compat ledger (groups only, extension state
/// ignored); `Ok(None)` when no ledger file exists. Called by
/// [`read_fission_ledger_for_session`] and read sites that don't need
/// detach/import/charter state.
pub fn read_fission_ledger(log_dir: &Path) -> io::Result<Option<FissionLedger>> {
    let path = ledger_path(log_dir);
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err),
    };
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(io::Error::other)
}

/// Connected-component view of the plain ledger for one session. Called by
/// the MCP `get_status` surface (`mcp.rs`) and the rewind-record snapshot
/// path (`main.rs`).
pub fn read_fission_ledger_for_session(
    log_dir: &Path,
    session_id: &str,
) -> io::Result<Option<FissionLedger>> {
    let Some(ledger) = read_fission_ledger(log_dir)? else {
        return Ok(None);
    };
    Ok(filter_ledger_for_session(ledger, session_id))
}

/// Persist a plain (extension-less) ledger view. Pre-existing public write
/// surface, kept for external callers that own only the back-compat groups;
/// the in-module mutators now write through
/// [`persist_fission_ledger_document`] instead.
#[allow(dead_code)] // retained compat surface; in-module writers use the document path
pub fn persist_fission_ledger(log_dir: &Path, ledger: &FissionLedger) -> io::Result<()> {
    // Read-modify-write: take the same lock as the other mutators.
    let _guard = ledger_write_lock();
    // The caller only owns the back-compat ledger section; preserve whatever
    // extension state is already on disk so a plain-ledger writer can't
    // silently drop detach/import/charter records. A corrupt or unreadable
    // existing file falls back to writing without `ext`, matching the old
    // overwrite-to-heal behavior instead of introducing a new failure mode.
    let ext = read_fission_ledger_document(log_dir)
        .ok()
        .flatten()
        .map(|document| document.ext)
        .unwrap_or_default();
    persist_fission_ledger_document(
        log_dir,
        &FissionLedgerDocument {
            groups: ledger.groups.clone(),
            ext,
        },
    )
}

/// Read the full ledger document (groups + extension state); `Ok(None)` when
/// no ledger file exists. The read entry point for callers that need
/// detach/import/charter state — dashboard and MCP status wiring.
pub fn read_fission_ledger_document(log_dir: &Path) -> io::Result<Option<FissionLedgerDocument>> {
    let path = ledger_path(log_dir);
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err),
    };
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(io::Error::other)
}

/// Session-filtered document view: groups are filtered with the same
/// connected-component rule as [`read_fission_ledger_for_session`], and
/// extension entries follow their groups. For per-session dashboard/MCP
/// surfaces.
#[allow(dead_code)] // wired by the fission/lineage integration stage
pub fn read_fission_ledger_document_for_session(
    log_dir: &Path,
    session_id: &str,
) -> io::Result<Option<FissionLedgerDocument>> {
    let Some(document) = read_fission_ledger_document(log_dir)? else {
        return Ok(None);
    };
    let FissionLedgerDocument { groups, mut ext } = document;
    let Some(ledger) = filter_ledger_for_session(FissionLedger { groups }, session_id) else {
        return Ok(None);
    };
    let keep: BTreeSet<&str> = ledger
        .groups
        .iter()
        .map(|group| group.group_id.as_str())
        .collect();
    ext.groups
        .retain(|group| keep.contains(group.group_id.as_str()));
    Ok(Some(FissionLedgerDocument {
        groups: ledger.groups,
        ext,
    }))
}

/// Persist the full document atomically (same crash-safety rationale as
/// [`persist_fission_ledger`]). Internal mutators and any future caller that
/// owns extension state write through this.
pub fn persist_fission_ledger_document(
    log_dir: &Path,
    document: &FissionLedgerDocument,
) -> io::Result<()> {
    fs::create_dir_all(log_dir)?;
    let bytes = serde_json::to_vec_pretty(document).map_err(io::Error::other)?;
    // Atomic write so a crash mid-write can't truncate the ledger into invalid
    // JSON that the read side would then silently drop.
    crate::file_watcher::atomic_write(&ledger_path(log_dir), &bytes)
}

/// Upsert the `(parent, anchor)` group from a passively observed collab /
/// sub-agent event. Called by the external-agent supervision hooks in
/// `main.rs` / `mcp.rs` whenever a spawn/wait/status event is seen.
/// `Ok(None)` when the observation lacks the identifying ids.
///
/// Status handling: a sticky `detached`/`cancelled` branch status is never
/// overwritten (a detach must survive stray completion events from a child
/// that is still running — see [`branch_status_is_sticky`]); a terminal
/// status is never downgraded by a later, coarser non-terminal observation;
/// and a branch arriving late into a detached group is recorded for
/// observability but enters directly as `detached`.
pub fn record_fission_observation(
    log_dir: &Path,
    observation: FissionObservation,
) -> io::Result<Option<FissionGroup>> {
    let _guard = ledger_write_lock();
    let parent_session_id = clean_string(&observation.parent_session_id);
    let anchor_item_id = clean_string(&observation.anchor_item_id);
    if parent_session_id.is_none() || anchor_item_id.is_none() {
        return Ok(None);
    }
    let parent_session_id = parent_session_id.unwrap();
    let anchor_item_id = anchor_item_id.unwrap();
    let tool = clean_string(&observation.tool).unwrap_or_else(|| "spawn_agent".to_string());
    let now = chrono::Utc::now().to_rfc3339();
    let group_id = group_id(&parent_session_id, &anchor_item_id);
    let mut document = read_fission_ledger_document(log_dir)?.unwrap_or_default();
    let group_is_detached = document
        .ext
        .group(&group_id)
        .is_some_and(FissionGroupExt::is_detached);
    let idx = document
        .groups
        .iter()
        .position(|group| group.group_id == group_id);
    let group = if let Some(idx) = idx {
        &mut document.groups[idx]
    } else {
        document.groups.push(FissionGroup {
            group_id: group_id.clone(),
            parent_session_id: parent_session_id.clone(),
            anchor_item_id: anchor_item_id.clone(),
            tool: tool.clone(),
            objective: clean_string(observation.prompt.as_deref().unwrap_or_default()),
            prompt: clean_string(observation.prompt.as_deref().unwrap_or_default()),
            created_at: now.clone(),
            updated_at: now.clone(),
            canonical_session_id: None,
            branches: Vec::new(),
        });
        document.groups.last_mut().expect("pushed group")
    };

    group.parent_session_id = parent_session_id;
    group.anchor_item_id = anchor_item_id;
    group.tool = tool;
    if let Some(prompt) = clean_string(observation.prompt.as_deref().unwrap_or_default()) {
        group.objective = Some(prompt.clone());
        group.prompt = Some(prompt);
    }
    group.updated_at = now.clone();

    for branch in observation.branches {
        let Some(session_id) = clean_string(&branch.session_id) else {
            continue;
        };
        if session_id == group.parent_session_id {
            continue;
        }
        let status = normalize_status(
            clean_string(&branch.status)
                .as_deref()
                .unwrap_or(&observation.status),
        );
        // A detached group cannot host live branches: its anchor left the
        // effective history, so a late-arriving child is recorded for
        // observability but enters directly as `detached`.
        let status = if group_is_detached {
            "detached".to_string()
        } else {
            status
        };
        let summary = clean_string(branch.summary.as_deref().unwrap_or_default());
        let raw_log = format!("session.jsonl#session_id={session_id}");
        let branch_idx = group
            .branches
            .iter()
            .position(|existing| existing.session_id == session_id);
        if let Some(idx) = branch_idx {
            let existing = &mut group.branches[idx];
            // Sticky statuses (`detached`/`cancelled`) survive any later
            // observation: a detached branch's child may still be running and
            // will eventually emit stray completion events that must not
            // resurrect or "complete" the branch behind the supervisor's
            // back. Beyond that, don't let a stale/coarser observation
            // downgrade a terminal status (e.g. a receiver-only
            // `wait`/`completed` collab call re-recording an already-completed
            // child as `running`).
            if !branch_status_is_sticky(&existing.status)
                && (!branch_status_is_terminal(&existing.status)
                    || branch_status_is_terminal(&status))
            {
                existing.status = status;
            }
            if summary.is_some() {
                existing.summary = summary;
            }
            if existing.task.is_none() {
                existing.task = group.objective.clone();
            }
            if existing.model.is_none() {
                existing.model = clean_string(observation.model.as_deref().unwrap_or_default());
            }
            if existing.reasoning_effort.is_none() {
                existing.reasoning_effort =
                    clean_string(observation.reasoning_effort.as_deref().unwrap_or_default());
            }
            existing.updated_at = now.clone();
        } else {
            group.branches.push(FissionBranch {
                backend_session_id: Some(session_id.clone()),
                status,
                summary,
                task: group.objective.clone(),
                model: clean_string(observation.model.as_deref().unwrap_or_default()),
                reasoning_effort: clean_string(
                    observation.reasoning_effort.as_deref().unwrap_or_default(),
                ),
                worktree_path: None,
                raw_log,
                ephemeral: false,
                updated_at: now.clone(),
                session_id,
            });
        }
    }
    group
        .branches
        .sort_by(|a, b| a.session_id.cmp(&b.session_id));
    let updated = group.clone();
    persist_fission_ledger_document(log_dir, &document)?;
    Ok(Some(updated))
}

/// First-writer-wins / compare-and-swap canonical claim. Kept for existing
/// call sites (the `claim_fission_canonical` MCP tool); it skips the
/// anchor-reachability check but still refuses detached groups and
/// detached/cancelled branches — a detached branch can never own canonical
/// continuation, no matter which surface the claim arrives through. New
/// wiring should prefer [`claim_canonical_checked`] with a real predicate.
pub fn claim_canonical(
    log_dir: &Path,
    group_id: &str,
    branch_session_id: &str,
    expected_canonical_session_id: Option<&str>,
) -> Result<FissionGroup, ClaimCanonicalError> {
    claim_canonical_checked(
        log_dir,
        group_id,
        branch_session_id,
        expected_canonical_session_id,
        |_| true,
    )
}

/// Claim a group's canonical branch while enforcing the invariant that a
/// branch can claim canonical continuation only at an anchor still reachable
/// from the active lineage. Called by the canonical-claim MCP tool / the
/// supervisor with a predicate over the effective (post-rewind) history:
/// `anchor_is_reachable` receives the group's `anchor_item_id` and answers
/// "is this item id still in effective history?".
///
/// Refusals beyond [`claim_canonical`]'s:
/// - [`ClaimCanonicalError::AnchorDetached`] when the group is already marked
///   detached or the predicate rejects its anchor;
/// - [`ClaimCanonicalError::BranchDetached`] when the claiming branch carries
///   a sticky `detached`/`cancelled` status.
pub fn claim_canonical_checked(
    log_dir: &Path,
    group_id: &str,
    branch_session_id: &str,
    expected_canonical_session_id: Option<&str>,
    anchor_is_reachable: impl Fn(&str) -> bool,
) -> Result<FissionGroup, ClaimCanonicalError> {
    let _guard = ledger_write_lock();
    let group_id = clean_string(group_id).unwrap_or_default();
    let branch_session_id = clean_string(branch_session_id).unwrap_or_default();
    let expected = expected_canonical_session_id.and_then(clean_string);
    let mut document = read_fission_ledger_document(log_dir)?.unwrap_or_default();
    let group_is_detached = document
        .ext
        .group(&group_id)
        .is_some_and(FissionGroupExt::is_detached);
    let group = document
        .groups
        .iter_mut()
        .find(|group| group.group_id == group_id)
        .ok_or_else(|| ClaimCanonicalError::GroupNotFound(group_id.clone()))?;
    if group_is_detached || !anchor_is_reachable(&group.anchor_item_id) {
        return Err(ClaimCanonicalError::AnchorDetached {
            group_id,
            anchor_item_id: group.anchor_item_id.clone(),
        });
    }
    let Some(branch) = group
        .branches
        .iter()
        .find(|branch| branch.session_id == branch_session_id)
    else {
        return Err(ClaimCanonicalError::BranchNotFound {
            group_id,
            branch_session_id,
        });
    };
    if branch_status_is_sticky(&branch.status) {
        return Err(ClaimCanonicalError::BranchDetached {
            group_id,
            branch_session_id,
            status: normalize_branch_status(&branch.status).to_string(),
        });
    }

    let current = group.canonical_session_id.clone();
    if let Some(expected) = expected {
        if current.as_deref() != Some(expected.as_str()) {
            return Err(ClaimCanonicalError::Conflict {
                group_id,
                expected: Some(expected),
                current,
            });
        }
    } else if current
        .as_deref()
        .is_some_and(|current| current != branch_session_id)
    {
        return Err(ClaimCanonicalError::Conflict {
            group_id,
            expected: None,
            current,
        });
    }

    group.canonical_session_id = Some(branch_session_id);
    group.updated_at = chrono::Utc::now().to_rfc3339();
    let updated = group.clone();
    persist_fission_ledger_document(log_dir, &document)?;
    Ok(updated)
}

/// Default `detach_reason` recorded by [`detach_groups_with_invalid_anchors`].
#[allow(dead_code)] // wired by the fission/lineage integration stage
pub const DETACH_REASON_ANCHOR_UNREACHABLE: &str = "anchor-unreachable";

/// Detach every fission group whose parent matches one of
/// `parent_session_id_candidates` and whose `anchor_item_id` fails
/// `anchor_is_reachable`. Called by the rewind path (the supervisor's
/// `apply_external_context_rewind`) right after a successful anchored
/// rollback: pass every id the rewound parent is known by (Intendant session
/// id and backend thread id — recording paths differ in which they store) and
/// a predicate answering "is this item id still in the effective post-rewind
/// history?".
///
/// Detaching a group flips every non-terminal branch to the sticky `detached`
/// status, stamps `detached_at` + `detach_reason`
/// ([`DETACH_REASON_ANCHOR_UNREACHABLE`]) on the group's extension entry, and
/// clears `canonical_session_id` if the canonical branch itself was detached
/// — a detached branch cannot own canonical continuation. Branches that
/// already reached a terminal status (`completed`, `failed`, …) keep it:
/// their recorded results stay real even though the join point is gone.
/// Already-detached groups are skipped, so the call is idempotent.
#[allow(dead_code)] // wired by the fission/lineage integration stage
pub fn detach_groups_with_invalid_anchors(
    log_dir: &Path,
    parent_session_id_candidates: &[String],
    anchor_is_reachable: impl Fn(&str) -> bool,
) -> io::Result<DetachReport> {
    let _guard = ledger_write_lock();
    let candidates: BTreeSet<&str> = parent_session_id_candidates
        .iter()
        .map(|id| id.trim())
        .filter(|id| !id.is_empty())
        .collect();
    let mut report = DetachReport::default();
    if candidates.is_empty() {
        return Ok(report);
    }
    let Some(mut document) = read_fission_ledger_document(log_dir)? else {
        return Ok(report);
    };
    let now = chrono::Utc::now().to_rfc3339();
    for group in document.groups.iter_mut() {
        if !candidates.contains(group.parent_session_id.as_str()) {
            continue;
        }
        if document
            .ext
            .group(&group.group_id)
            .is_some_and(FissionGroupExt::is_detached)
        {
            // Idempotence: an already-detached group keeps its original
            // detached_at/detach_reason and is not re-reported.
            continue;
        }
        if anchor_is_reachable(&group.anchor_item_id) {
            continue;
        }
        detach_group_in_place(
            group,
            &mut document.ext,
            DETACH_REASON_ANCHOR_UNREACHABLE,
            &now,
            &mut report,
        );
    }
    if !report.detached_group_ids.is_empty() {
        persist_fission_ledger_document(log_dir, &document)?;
    }
    Ok(report)
}

/// Detach a single group by id, with a caller-supplied reason. For explicit
/// "sever this group" intents (dashboard action, MCP tool, model-driven
/// cancellation flows). Same semantics as
/// [`detach_groups_with_invalid_anchors`] for the one group; idempotent —
/// re-detaching returns the group unchanged and keeps the original
/// `detached_at`/`detach_reason`. `ErrorKind::NotFound` when the group does
/// not exist.
#[allow(dead_code)] // wired by the fission/lineage integration stage
pub fn detach_group(log_dir: &Path, group_id: &str, reason: &str) -> io::Result<FissionGroup> {
    let _guard = ledger_write_lock();
    let group_id = clean_string(group_id).unwrap_or_default();
    let mut document = read_fission_ledger_document(log_dir)?.unwrap_or_default();
    let already_detached = document
        .ext
        .group(&group_id)
        .is_some_and(FissionGroupExt::is_detached);
    let Some(group) = document
        .groups
        .iter_mut()
        .find(|group| group.group_id == group_id)
    else {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("fission group `{group_id}` was not found"),
        ));
    };
    if already_detached {
        return Ok(group.clone());
    }
    let now = chrono::Utc::now().to_rfc3339();
    let mut report = DetachReport::default();
    detach_group_in_place(group, &mut document.ext, reason, &now, &mut report);
    let updated = group.clone();
    persist_fission_ledger_document(log_dir, &document)?;
    Ok(updated)
}

/// Shared detach core: flips non-terminal branches to `detached`, stamps the
/// group's extension entry, clears a detached canonical claim, and records
/// what changed into `report`.
fn detach_group_in_place(
    group: &mut FissionGroup,
    ext: &mut FissionLedgerExt,
    reason: &str,
    now: &str,
    report: &mut DetachReport,
) {
    let group_ext = ext.group_mut_or_insert(&group.group_id);
    group_ext.detached_at = Some(now.to_string());
    group_ext.detach_reason = clean_string(reason);
    for branch in group.branches.iter_mut() {
        if branch_status_is_terminal(&branch.status) {
            continue;
        }
        branch.status = "detached".to_string();
        branch.updated_at = now.to_string();
        report
            .detached_branch_session_ids
            .push(branch.session_id.clone());
    }
    if let Some(canonical) = group.canonical_session_id.clone() {
        // A detached branch cannot own canonical continuation; a canonical
        // branch that finished before the detach keeps its claim (the claim
        // happened at a then-valid anchor and its result is still real).
        let canonical_detached = group.branches.iter().any(|branch| {
            branch.session_id == canonical && normalize_branch_status(&branch.status) == "detached"
        });
        if canonical_detached {
            report.cleared_canonicals.push(ClearedCanonical {
                group_id: group.group_id.clone(),
                canonical_session_id: canonical,
            });
            group.canonical_session_id = None;
        }
    }
    group.updated_at = now.to_string();
    report.detached_group_ids.push(group.group_id.clone());
}

/// Record that a branch's result was explicitly imported into the active
/// lineage. Called by the import flow (MCP tool / dashboard "import result"
/// action) after the parent pulled a branch's artifact or summary into its
/// own continuation. Stamps `imported_at` on the branch's extension entry
/// (re-importing refreshes it) and updates the summary when given.
///
/// Deliberately does NOT touch the branch status: import is artifact-level,
/// so a detached branch stays detached — the ledger keeps showing that the
/// branch's join point is gone even though its result was salvaged.
/// `ErrorKind::NotFound` when the group or branch does not exist.
#[allow(dead_code)] // wired by the fission/lineage integration stage
pub fn mark_branch_imported(
    log_dir: &Path,
    group_id: &str,
    branch_session_id: &str,
    summary: Option<&str>,
) -> io::Result<FissionGroup> {
    let _guard = ledger_write_lock();
    let group_id = clean_string(group_id).unwrap_or_default();
    let branch_session_id = clean_string(branch_session_id).unwrap_or_default();
    let mut document = read_fission_ledger_document(log_dir)?.unwrap_or_default();
    let now = chrono::Utc::now().to_rfc3339();
    let Some(group) = document
        .groups
        .iter_mut()
        .find(|group| group.group_id == group_id)
    else {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("fission group `{group_id}` was not found"),
        ));
    };
    let Some(branch) = group
        .branches
        .iter_mut()
        .find(|branch| branch.session_id == branch_session_id)
    else {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("branch `{branch_session_id}` is not part of fission group `{group_id}`"),
        ));
    };
    if let Some(summary) = summary.and_then(clean_string) {
        branch.summary = Some(summary);
    }
    branch.updated_at = now.clone();
    group.updated_at = now.clone();
    let updated = group.clone();
    document
        .ext
        .group_mut_or_insert(&group_id)
        .branch_mut_or_insert(&branch_session_id)
        .imported_at = Some(now);
    persist_fission_ledger_document(log_dir, &document)?;
    Ok(updated)
}

/// Record work metadata a branch reported (changed files, tests run) and
/// optionally refresh its summary. Called by the supervisor when a child
/// reports progress or completes (collab progress events, or the
/// branch-report MCP tool a later stage adds). Lists are merged as a
/// first-seen-ordered, deduplicated union — children may report cumulatively
/// or in deltas without losing earlier facts; an empty list leaves the stored
/// list unchanged. Even a metadata-free call bumps the branch's `updated_at`
/// (heartbeat). `ErrorKind::NotFound` when the group or branch does not
/// exist.
#[allow(dead_code)] // wired by the fission/lineage integration stage
pub fn update_branch_work(
    log_dir: &Path,
    group_id: &str,
    branch_session_id: &str,
    changed_files: &[String],
    tests_run: &[String],
    summary: Option<&str>,
) -> io::Result<FissionGroup> {
    let _guard = ledger_write_lock();
    let group_id = clean_string(group_id).unwrap_or_default();
    let branch_session_id = clean_string(branch_session_id).unwrap_or_default();
    let mut document = read_fission_ledger_document(log_dir)?.unwrap_or_default();
    let now = chrono::Utc::now().to_rfc3339();
    let Some(group) = document
        .groups
        .iter_mut()
        .find(|group| group.group_id == group_id)
    else {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("fission group `{group_id}` was not found"),
        ));
    };
    let Some(branch) = group
        .branches
        .iter_mut()
        .find(|branch| branch.session_id == branch_session_id)
    else {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("branch `{branch_session_id}` is not part of fission group `{group_id}`"),
        ));
    };
    if let Some(summary) = summary.and_then(clean_string) {
        branch.summary = Some(summary);
    }
    branch.updated_at = now.clone();
    group.updated_at = now;
    let updated = group.clone();
    let branch_ext = document
        .ext
        .group_mut_or_insert(&group_id)
        .branch_mut_or_insert(&branch_session_id);
    merge_unique(&mut branch_ext.changed_files, changed_files);
    merge_unique(&mut branch_ext.tests_run, tests_run);
    persist_fission_ledger_document(log_dir, &document)?;
    Ok(updated)
}

/// Register a model-driven fission branch at its exact spawn anchor. Called by
/// the spawn MCP tool (added in a later stage) immediately after launching a
/// sibling/fork on the model's behalf, before any collab observation can
/// arrive. Creates the `(parent, anchor)` group if needed — same keying as
/// [`record_fission_observation`], so later observed collab events for the
/// same anchor merge into the same group — upserts the branch with status
/// `running`, and stores the charter on the branch's extension entry.
///
/// Group-level `objective`/`prompt` are filled by the first writer only:
/// charters are per-branch, so a sibling's later registration must not
/// clobber them. Re-registering an existing branch (e.g. a retried spawn)
/// treats the explicit metadata as authoritative but never downgrades a
/// terminal or sticky status. Refuses (`ErrorKind::InvalidInput`) to register
/// into a detached group: a severed anchor cannot host new live branches.
#[allow(dead_code)] // wired by the fission/lineage integration stage
pub fn register_spawned_branch(
    log_dir: &Path,
    parent_session_id: &str,
    anchor_item_id: &str,
    charter: BranchCharter,
    branch: NewSpawnedBranch,
) -> io::Result<FissionGroup> {
    let _guard = ledger_write_lock();
    let parent_session_id = clean_string(parent_session_id).ok_or_else(|| {
        invalid_input("register_spawned_branch requires a non-empty parent_session_id")
    })?;
    let anchor_item_id = clean_string(anchor_item_id).ok_or_else(|| {
        invalid_input("register_spawned_branch requires a non-empty anchor_item_id")
    })?;
    let session_id = clean_string(&branch.session_id).ok_or_else(|| {
        invalid_input("register_spawned_branch requires a non-empty branch session_id")
    })?;
    if session_id == parent_session_id {
        return Err(invalid_input(
            "a fission branch cannot be its own parent session",
        ));
    }
    let objective = clean_string(&charter.objective).ok_or_else(|| {
        invalid_input("register_spawned_branch requires a non-empty charter objective")
    })?;
    let charter = BranchCharter {
        objective: objective.clone(),
        write_scope: charter.write_scope.as_deref().and_then(clean_string),
        worktree_requested: charter.worktree_requested,
    };
    let now = chrono::Utc::now().to_rfc3339();
    let group_id = group_id(&parent_session_id, &anchor_item_id);
    let mut document = read_fission_ledger_document(log_dir)?.unwrap_or_default();
    if document
        .ext
        .group(&group_id)
        .is_some_and(FissionGroupExt::is_detached)
    {
        return Err(invalid_input(format!(
            "fission group `{group_id}` is detached; a severed anchor cannot host new branches"
        )));
    }
    let idx = document
        .groups
        .iter()
        .position(|group| group.group_id == group_id);
    let group = if let Some(idx) = idx {
        &mut document.groups[idx]
    } else {
        document.groups.push(FissionGroup {
            group_id: group_id.clone(),
            parent_session_id: parent_session_id.clone(),
            anchor_item_id: anchor_item_id.clone(),
            // Match the observation default so groups look uniform no matter
            // which path created them, and later collab observations for the
            // same anchor merge cleanly.
            tool: "spawn_agent".to_string(),
            objective: None,
            prompt: None,
            created_at: now.clone(),
            updated_at: now.clone(),
            canonical_session_id: None,
            branches: Vec::new(),
        });
        document.groups.last_mut().expect("pushed group")
    };
    let task = branch.task.as_deref().and_then(clean_string);
    if group.objective.is_none() {
        group.objective = Some(objective.clone());
    }
    if group.prompt.is_none() {
        group.prompt = task.clone().or_else(|| Some(objective.clone()));
    }
    group.updated_at = now.clone();
    let backend_session_id = branch.backend_session_id.as_deref().and_then(clean_string);
    let model = branch.model.as_deref().and_then(clean_string);
    let reasoning_effort = branch.reasoning_effort.as_deref().and_then(clean_string);
    let raw_log = branch.raw_log.as_deref().and_then(clean_string);
    let worktree_path = branch.worktree_path.clone();
    if let Some(existing) = group
        .branches
        .iter_mut()
        .find(|existing| existing.session_id == session_id)
    {
        if !branch_status_is_sticky(&existing.status)
            && !branch_status_is_terminal(&existing.status)
        {
            existing.status = "running".to_string();
        }
        if backend_session_id.is_some() {
            existing.backend_session_id = backend_session_id;
        }
        if task.is_some() {
            existing.task = task;
        }
        if model.is_some() {
            existing.model = model;
        }
        if reasoning_effort.is_some() {
            existing.reasoning_effort = reasoning_effort;
        }
        if worktree_path.is_some() {
            existing.worktree_path = worktree_path;
        }
        if let Some(raw_log) = raw_log {
            existing.raw_log = raw_log;
        }
        existing.updated_at = now.clone();
    } else {
        group.branches.push(FissionBranch {
            session_id: session_id.clone(),
            backend_session_id,
            status: "running".to_string(),
            summary: None,
            task: task.or_else(|| Some(objective.clone())),
            model,
            reasoning_effort,
            worktree_path,
            raw_log: raw_log.unwrap_or_else(|| format!("session.jsonl#session_id={session_id}")),
            ephemeral: false,
            updated_at: now.clone(),
        });
        group
            .branches
            .sort_by(|a, b| a.session_id.cmp(&b.session_id));
    }
    let updated = group.clone();
    document
        .ext
        .group_mut_or_insert(&group_id)
        .branch_mut_or_insert(&session_id)
        .charter = Some(charter);
    persist_fission_ledger_document(log_dir, &document)?;
    Ok(updated)
}

fn filter_ledger_for_session(ledger: FissionLedger, session_id: &str) -> Option<FissionLedger> {
    let session_id = session_id.trim();
    if session_id.is_empty() {
        return if ledger.groups.is_empty() {
            None
        } else {
            Some(ledger)
        };
    }

    let mut related: BTreeSet<String> = [session_id.to_string()].into_iter().collect();
    loop {
        let before = related.len();
        for group in &ledger.groups {
            let group_touches_related = related.contains(&group.parent_session_id)
                || group
                    .canonical_session_id
                    .as_ref()
                    .is_some_and(|id| related.contains(id))
                || group
                    .branches
                    .iter()
                    .any(|branch| related.contains(&branch.session_id));
            if group_touches_related {
                related.insert(group.parent_session_id.clone());
                if let Some(canonical) = &group.canonical_session_id {
                    related.insert(canonical.clone());
                }
                for branch in &group.branches {
                    related.insert(branch.session_id.clone());
                }
            }
        }
        if related.len() == before {
            break;
        }
    }

    let groups: Vec<FissionGroup> = ledger
        .groups
        .into_iter()
        .filter(|group| {
            related.contains(&group.parent_session_id)
                || group
                    .canonical_session_id
                    .as_ref()
                    .is_some_and(|id| related.contains(id))
                || group
                    .branches
                    .iter()
                    .any(|branch| related.contains(&branch.session_id))
        })
        .collect();
    if groups.is_empty() {
        None
    } else {
        Some(FissionLedger { groups })
    }
}

fn normalize_status(status: &str) -> String {
    match status.trim() {
        "inProgress" | "pendingInit" => "running".to_string(),
        "errored" => "failed".to_string(),
        // `notFound` is frequently transient (a state lookup miss while a child is
        // starting/migrating); treat it as non-terminal `unknown` rather than
        // conflating it with a definitive failure.
        "notFound" => "unknown".to_string(),
        "shutdown" => "ended".to_string(),
        "completed" | "interrupted" | "failed" | "running" => status.trim().to_string(),
        other if !other.is_empty() => other.to_string(),
        _ => "running".to_string(),
    }
}

fn clean_string(value: &str) -> Option<String> {
    let value = value.trim();
    if value.is_empty() {
        None
    } else {
        Some(value.to_string())
    }
}

/// Merge `additions` into `target` as a first-seen-ordered, deduplicated
/// union, ignoring empty/whitespace-only entries. Lets children report
/// cumulatively or in deltas without losing earlier facts (see
/// [`update_branch_work`]).
fn merge_unique(target: &mut Vec<String>, additions: &[String]) {
    for addition in additions {
        let Some(addition) = clean_string(addition) else {
            continue;
        };
        if !target.contains(&addition) {
            target.push(addition);
        }
    }
}

/// Uniform `InvalidInput` error for the validation paths.
fn invalid_input(message: impl Into<Box<dyn std::error::Error + Send + Sync>>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

/// FNV-1a 64-bit fold; deterministic across processes and crate versions
/// (unlike `std`'s `DefaultHasher`), so it is safe for a persisted, equality-
/// matched key.
fn fnv1a(mut hash: u64, bytes: &[u8]) -> u64 {
    for &byte in bytes {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// Stable hex hash of the exact (parent, anchor) bytes, length-prefixed so a
/// byte that straddles the field boundary can't forge a collision.
fn stable_pair_hash(parent_session_id: &str, anchor_item_id: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    hash = fnv1a(hash, &(parent_session_id.len() as u64).to_le_bytes());
    hash = fnv1a(hash, parent_session_id.as_bytes());
    hash = fnv1a(hash, &(anchor_item_id.len() as u64).to_le_bytes());
    hash = fnv1a(hash, anchor_item_id.as_bytes());
    format!("{hash:016x}")
}

/// Project a stored or observed branch status onto the canonical vocabulary
/// (`running | blocked | completed | failed | detached | cancelled`, see
/// [`FissionBranch::status`]):
///
/// - `inProgress` / `pendingInit` / `unknown` / `notFound`, the empty string,
///   and unrecognized values are non-terminal and map to `running`;
/// - `errored` maps to `failed`;
/// - `ended` / `shutdown` (a child whose lifecycle finished) map to
///   `completed`;
/// - `interrupted` / `canceled` (a child stopped before completion) map to
///   `cancelled`;
/// - canonical values pass through unchanged.
///
/// Called by status-display consumers (the dashboard Managed tab and MCP
/// fission status surfaces) that want a closed set to switch on, and
/// internally by [`branch_status_is_terminal`] and the detach/claim paths.
/// Stickiness is deliberately *not* derivable from the normalized value: use
/// [`branch_status_is_sticky`] (exact raw match), because a legacy
/// `interrupted` displays as `cancelled` yet must stay upgradeable by a real
/// completion observation.
pub fn normalize_branch_status(status: &str) -> &'static str {
    match status.trim() {
        "blocked" => "blocked",
        "completed" | "ended" | "shutdown" => "completed",
        "failed" | "errored" => "failed",
        "detached" => "detached",
        "cancelled" | "canceled" | "interrupted" => "cancelled",
        _ => "running",
    }
}

/// True for statuses whose branch lifecycle is finished: `completed`,
/// `failed`, `detached`, or `cancelled` after [`normalize_branch_status`]
/// folding (so the legacy `ended` / `interrupted` count too). A later,
/// coarser observation must not downgrade a terminal status back to
/// `running`, and the detach paths skip terminal branches so their recorded
/// results stay real even when the join point is severed. Called by
/// [`record_fission_observation`], [`register_spawned_branch`],
/// [`detach_groups_with_invalid_anchors`], and [`detach_group`].
pub fn branch_status_is_terminal(status: &str) -> bool {
    matches!(
        normalize_branch_status(status),
        "completed" | "failed" | "detached" | "cancelled"
    )
}

/// True for the *sticky* statuses — exactly `detached` or `cancelled`, raw
/// value. Sticky statuses are written only by explicit supervisor APIs (the
/// detach functions, a future cancel/reattach API) and no passive observation
/// may overwrite them, not even with another terminal status: a detached
/// branch's child process may still be running and will eventually emit stray
/// completion events that must not resurrect or "complete" the branch behind
/// the supervisor's back. Matches the raw value rather than the normalized
/// one on purpose — the observation-recorded legacy `interrupted` normalizes
/// to `cancelled` for display but keeps its pre-vocabulary upgradeability.
/// Called by [`record_fission_observation`], [`register_spawned_branch`], and
/// [`claim_canonical_checked`].
pub fn branch_status_is_sticky(status: &str) -> bool {
    matches!(status.trim(), "detached" | "cancelled")
}

fn stable_slug(value: &str) -> String {
    let mut out = String::new();
    for ch in value.chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_') {
            out.push(ch);
        } else {
            out.push('_');
        }
    }
    let out = out.trim_matches('_');
    if out.is_empty() {
        "unknown".to_string()
    } else {
        out.chars().take(96).collect()
    }
}

fn display_optional_id(value: &Option<String>) -> &str {
    value.as_deref().unwrap_or("<none>")
}

fn ledger_write_lock() -> MutexGuard<'static, ()> {
    static LEDGER_WRITE_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LEDGER_WRITE_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn observation(status: &str) -> FissionObservation {
        FissionObservation {
            parent_session_id: "parent".to_string(),
            anchor_item_id: "call-123".to_string(),
            tool: "spawn_agent".to_string(),
            status: status.to_string(),
            prompt: Some("inspect parser".to_string()),
            model: Some("gpt-5.2-codex".to_string()),
            reasoning_effort: Some("high".to_string()),
            branches: vec![FissionBranchObservation {
                session_id: "child".to_string(),
                status: status.to_string(),
                summary: None,
            }],
        }
    }

    #[test]
    fn records_fission_group_by_exact_spawn_anchor() {
        let dir = tempdir().unwrap();
        let group = record_fission_observation(dir.path(), observation("inProgress"))
            .unwrap()
            .expect("group");

        assert_eq!(group.group_id, group_id("parent", "call-123"));
        assert_eq!(group.parent_session_id, "parent");
        assert_eq!(group.anchor_item_id, "call-123");
        assert_eq!(group.objective.as_deref(), Some("inspect parser"));
        assert_eq!(group.branches.len(), 1);
        assert_eq!(group.branches[0].session_id, "child");
        assert_eq!(group.branches[0].status, "running");

        let ledger = read_fission_ledger(dir.path()).unwrap().expect("ledger");
        assert_eq!(ledger.groups, vec![group]);
    }

    #[test]
    fn updates_existing_branch_status_and_summary() {
        let dir = tempdir().unwrap();
        record_fission_observation(dir.path(), observation("inProgress")).unwrap();
        let mut done = observation("completed");
        done.branches[0].summary = Some("parser is fine".to_string());
        let group = record_fission_observation(dir.path(), done)
            .unwrap()
            .expect("group");

        assert_eq!(group.branches.len(), 1);
        assert_eq!(group.branches[0].status, "completed");
        assert_eq!(group.branches[0].summary.as_deref(), Some("parser is fine"));
    }

    #[test]
    fn terminal_status_is_not_downgraded_by_later_running_observation() {
        let dir = tempdir().unwrap();
        let mut done = observation("completed");
        done.branches[0].summary = Some("done".to_string());
        record_fission_observation(dir.path(), done).unwrap();
        // A later coarser observation reports the child as running again.
        let group = record_fission_observation(dir.path(), observation("running"))
            .unwrap()
            .expect("group");
        assert_eq!(group.branches[0].status, "completed");
        assert_eq!(group.branches[0].summary.as_deref(), Some("done"));
    }

    #[test]
    fn group_id_is_collision_resistant_across_separator_ambiguity() {
        // (x, y-z) and (x-y, z) slug to the same "fission-x-y-z" prefix; the hash
        // suffix must keep the two group ids distinct.
        assert_ne!(group_id("x", "y-z"), group_id("x-y", "z"));
    }

    #[test]
    fn canonical_claim_is_first_writer_wins_without_expected_id() {
        let dir = tempdir().unwrap();
        record_fission_observation(dir.path(), observation("running")).unwrap();
        let gid = group_id("parent", "call-123");
        let claimed = claim_canonical(dir.path(), &gid, "child", None).unwrap();
        assert_eq!(claimed.canonical_session_id.as_deref(), Some("child"));

        let err = claim_canonical(dir.path(), &gid, "other", None).unwrap_err();
        assert!(matches!(err, ClaimCanonicalError::BranchNotFound { .. }));
    }

    #[test]
    fn canonical_claim_honors_compare_and_swap() {
        let dir = tempdir().unwrap();
        let mut obs = observation("running");
        obs.branches.push(FissionBranchObservation {
            session_id: "child-2".to_string(),
            status: "running".to_string(),
            summary: None,
        });
        record_fission_observation(dir.path(), obs).unwrap();
        let gid = group_id("parent", "call-123");
        claim_canonical(dir.path(), &gid, "child", None).unwrap();

        let err = claim_canonical(dir.path(), &gid, "child-2", Some("child-2")).unwrap_err();
        assert!(matches!(err, ClaimCanonicalError::Conflict { .. }));

        let claimed = claim_canonical(dir.path(), &gid, "child-2", Some("child")).unwrap();
        assert_eq!(claimed.canonical_session_id.as_deref(), Some("child-2"));
    }

    #[test]
    fn filters_ledger_to_related_session_component() {
        let dir = tempdir().unwrap();
        record_fission_observation(dir.path(), observation("running")).unwrap();
        let unrelated = FissionObservation {
            parent_session_id: "other-parent".to_string(),
            anchor_item_id: "other-call".to_string(),
            tool: "spawn_agent".to_string(),
            status: "running".to_string(),
            prompt: None,
            model: None,
            reasoning_effort: None,
            branches: vec![FissionBranchObservation {
                session_id: "other-child".to_string(),
                status: "running".to_string(),
                summary: None,
            }],
        };
        record_fission_observation(dir.path(), unrelated).unwrap();

        let ledger = read_fission_ledger_for_session(dir.path(), "child")
            .unwrap()
            .expect("ledger");
        assert_eq!(ledger.groups.len(), 1);
        assert_eq!(ledger.groups[0].parent_session_id, "parent");
    }

    #[test]
    fn branch_status_vocabulary_normalizes_and_classifies() {
        assert_eq!(normalize_branch_status("running"), "running");
        assert_eq!(normalize_branch_status("inProgress"), "running");
        assert_eq!(normalize_branch_status("pendingInit"), "running");
        assert_eq!(normalize_branch_status("unknown"), "running");
        assert_eq!(normalize_branch_status("notFound"), "running");
        assert_eq!(normalize_branch_status(""), "running");
        assert_eq!(normalize_branch_status("something-new"), "running");
        assert_eq!(normalize_branch_status("blocked"), "blocked");
        assert_eq!(normalize_branch_status("ended"), "completed");
        assert_eq!(normalize_branch_status("shutdown"), "completed");
        assert_eq!(normalize_branch_status("errored"), "failed");
        assert_eq!(normalize_branch_status(" detached "), "detached");
        assert_eq!(normalize_branch_status("interrupted"), "cancelled");
        assert_eq!(normalize_branch_status("canceled"), "cancelled");

        for status in [
            "completed",
            "failed",
            "detached",
            "cancelled",
            "ended",
            "interrupted",
        ] {
            assert!(
                branch_status_is_terminal(status),
                "{status} should be terminal"
            );
        }
        for status in ["running", "blocked", "unknown", "inProgress", ""] {
            assert!(
                !branch_status_is_terminal(status),
                "{status} should not be terminal"
            );
        }

        assert!(branch_status_is_sticky("detached"));
        assert!(branch_status_is_sticky(" cancelled "));
        // Legacy `interrupted` normalizes to `cancelled` for display but is
        // not sticky: a real completion observation may still upgrade it.
        assert!(!branch_status_is_sticky("interrupted"));
        assert!(!branch_status_is_sticky("completed"));
        assert!(!branch_status_is_sticky("running"));
    }

    #[test]
    fn detach_flips_non_terminal_branches_and_keeps_terminal_results() {
        let dir = tempdir().unwrap();
        let mut obs = observation("running");
        obs.branches.push(FissionBranchObservation {
            session_id: "child-done".to_string(),
            status: "completed".to_string(),
            summary: Some("finished early".to_string()),
        });
        record_fission_observation(dir.path(), obs).unwrap();
        let gid = group_id("parent", "call-123");

        let report =
            detach_groups_with_invalid_anchors(dir.path(), &["parent".to_string()], |_| false)
                .unwrap();

        assert_eq!(report.detached_group_ids, vec![gid.clone()]);
        assert_eq!(
            report.detached_branch_session_ids,
            vec!["child".to_string()]
        );
        assert!(report.cleared_canonicals.is_empty());
        assert!(!report.is_empty());

        let document = read_fission_ledger_document(dir.path())
            .unwrap()
            .expect("document");
        let group = &document.groups[0];
        let child = group
            .branches
            .iter()
            .find(|branch| branch.session_id == "child")
            .unwrap();
        let done = group
            .branches
            .iter()
            .find(|branch| branch.session_id == "child-done")
            .unwrap();
        assert_eq!(child.status, "detached");
        // A branch that finished before the detach keeps its recorded result.
        assert_eq!(done.status, "completed");
        let ext = document.group_ext(&gid).expect("group ext");
        assert!(ext.detached_at.is_some());
        assert_eq!(
            ext.detach_reason.as_deref(),
            Some(DETACH_REASON_ANCHOR_UNREACHABLE)
        );
        assert!(document.group_is_detached(&gid));
    }

    #[test]
    fn detach_skips_reachable_anchors_and_foreign_parents() {
        let dir = tempdir().unwrap();
        record_fission_observation(dir.path(), observation("running")).unwrap();
        let mut foreign = observation("running");
        foreign.parent_session_id = "other-parent".to_string();
        foreign.branches[0].session_id = "other-child".to_string();
        record_fission_observation(dir.path(), foreign).unwrap();

        // Reachable anchor: nothing detaches even though the parent matches.
        let report =
            detach_groups_with_invalid_anchors(dir.path(), &["parent".to_string()], |anchor| {
                anchor == "call-123"
            })
            .unwrap();
        assert!(report.is_empty());

        // Unreachable anchor, but only `parent`'s group is a candidate.
        let report =
            detach_groups_with_invalid_anchors(dir.path(), &["parent".to_string()], |_| false)
                .unwrap();
        assert_eq!(
            report.detached_group_ids,
            vec![group_id("parent", "call-123")]
        );
        let document = read_fission_ledger_document(dir.path()).unwrap().unwrap();
        assert!(!document.group_is_detached(&group_id("other-parent", "call-123")));

        // No candidates: trivially a no-op.
        let report = detach_groups_with_invalid_anchors(dir.path(), &[], |_| false).unwrap();
        assert!(report.is_empty());
    }

    #[test]
    fn detach_is_idempotent_and_preserves_first_detach_metadata() {
        let dir = tempdir().unwrap();
        record_fission_observation(dir.path(), observation("running")).unwrap();
        let gid = group_id("parent", "call-123");
        detach_group(dir.path(), &gid, "rewind dropped the anchor").unwrap();
        let first = read_fission_ledger_document(dir.path()).unwrap().unwrap();
        let first_ext = first.group_ext(&gid).unwrap().clone();

        // Re-detaching by anchor scan reports nothing and changes nothing.
        let report =
            detach_groups_with_invalid_anchors(dir.path(), &["parent".to_string()], |_| false)
                .unwrap();
        assert!(report.is_empty());
        // Re-detaching directly keeps the original timestamp and reason.
        detach_group(dir.path(), &gid, "a different reason").unwrap();
        let second = read_fission_ledger_document(dir.path()).unwrap().unwrap();
        assert_eq!(second.group_ext(&gid), Some(&first_ext));

        // Detaching a missing group is a NotFound error, not a silent no-op.
        let err = detach_group(dir.path(), "missing", "reason").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn detach_is_sticky_against_late_observations() {
        let dir = tempdir().unwrap();
        record_fission_observation(dir.path(), observation("running")).unwrap();
        let gid = group_id("parent", "call-123");
        detach_group(dir.path(), &gid, "anchor rewound").unwrap();

        // The detached branch's child process is still running and eventually
        // reports completion; the stray event must not resurrect the branch.
        let mut late = observation("completed");
        late.branches[0].summary = Some("finished after detach".to_string());
        late.branches.push(FissionBranchObservation {
            session_id: "late-child".to_string(),
            status: "running".to_string(),
            summary: None,
        });
        let group = record_fission_observation(dir.path(), late)
            .unwrap()
            .expect("group");
        let child = group
            .branches
            .iter()
            .find(|branch| branch.session_id == "child")
            .unwrap();
        assert_eq!(child.status, "detached");
        // Only the status is sticky; the summary still lands for observability.
        assert_eq!(child.summary.as_deref(), Some("finished after detach"));
        // Late-arriving siblings of a severed anchor are recorded for
        // observability but enter directly as detached.
        let late_child = group
            .branches
            .iter()
            .find(|branch| branch.session_id == "late-child")
            .unwrap();
        assert_eq!(late_child.status, "detached");
    }

    #[test]
    fn detach_clears_canonical_when_canonical_branch_is_detached() {
        let dir = tempdir().unwrap();
        record_fission_observation(dir.path(), observation("running")).unwrap();
        let gid = group_id("parent", "call-123");
        claim_canonical(dir.path(), &gid, "child", None).unwrap();

        let report =
            detach_groups_with_invalid_anchors(dir.path(), &["parent".to_string()], |_| false)
                .unwrap();
        assert_eq!(
            report.cleared_canonicals,
            vec![ClearedCanonical {
                group_id: gid.clone(),
                canonical_session_id: "child".to_string(),
            }]
        );
        let ledger = read_fission_ledger(dir.path()).unwrap().unwrap();
        assert_eq!(ledger.groups[0].canonical_session_id, None);
    }

    #[test]
    fn detach_keeps_canonical_claimed_by_completed_branch() {
        let dir = tempdir().unwrap();
        record_fission_observation(dir.path(), observation("completed")).unwrap();
        let gid = group_id("parent", "call-123");
        claim_canonical(dir.path(), &gid, "child", None).unwrap();

        let report =
            detach_groups_with_invalid_anchors(dir.path(), &["parent".to_string()], |_| false)
                .unwrap();
        assert_eq!(report.detached_group_ids, vec![gid.clone()]);
        // The claim happened at a then-valid anchor and the result is real.
        assert!(report.cleared_canonicals.is_empty());
        let ledger = read_fission_ledger(dir.path()).unwrap().unwrap();
        assert_eq!(
            ledger.groups[0].canonical_session_id.as_deref(),
            Some("child")
        );
    }

    #[test]
    fn checked_claim_refuses_unreachable_anchor() {
        let dir = tempdir().unwrap();
        record_fission_observation(dir.path(), observation("running")).unwrap();
        let gid = group_id("parent", "call-123");
        let err = claim_canonical_checked(dir.path(), &gid, "child", None, |_| false).unwrap_err();
        match err {
            ClaimCanonicalError::AnchorDetached {
                group_id,
                anchor_item_id,
            } => {
                assert_eq!(group_id, gid);
                assert_eq!(anchor_item_id, "call-123");
            }
            other => panic!("expected AnchorDetached, got {other}"),
        }
        // Nothing was claimed.
        let ledger = read_fission_ledger(dir.path()).unwrap().unwrap();
        assert_eq!(ledger.groups[0].canonical_session_id, None);
    }

    #[test]
    fn checked_claim_refuses_detached_group_even_with_reachable_anchor() {
        let dir = tempdir().unwrap();
        record_fission_observation(dir.path(), observation("running")).unwrap();
        let gid = group_id("parent", "call-123");
        detach_group(dir.path(), &gid, "severed").unwrap();
        let err = claim_canonical_checked(dir.path(), &gid, "child", None, |_| true).unwrap_err();
        assert!(matches!(err, ClaimCanonicalError::AnchorDetached { .. }));
        // The legacy surface (always-true predicate) refuses the detached
        // group too: no surface can claim canonical at a severed anchor.
        let err = claim_canonical(dir.path(), &gid, "child", None).unwrap_err();
        assert!(matches!(err, ClaimCanonicalError::AnchorDetached { .. }));
    }

    #[test]
    fn checked_claim_refuses_sticky_branch() {
        let dir = tempdir().unwrap();
        let mut obs = observation("running");
        obs.branches.push(FissionBranchObservation {
            session_id: "child-cancelled".to_string(),
            status: "cancelled".to_string(),
            summary: None,
        });
        record_fission_observation(dir.path(), obs).unwrap();
        let gid = group_id("parent", "call-123");
        let err = claim_canonical_checked(dir.path(), &gid, "child-cancelled", None, |_| true)
            .unwrap_err();
        match err {
            ClaimCanonicalError::BranchDetached {
                group_id,
                branch_session_id,
                status,
            } => {
                assert_eq!(group_id, gid);
                assert_eq!(branch_session_id, "child-cancelled");
                assert_eq!(status, "cancelled");
            }
            other => panic!("expected BranchDetached, got {other}"),
        }
        // A healthy sibling can still claim through the same checked surface.
        let claimed = claim_canonical_checked(dir.path(), &gid, "child", None, |anchor| {
            anchor == "call-123"
        })
        .unwrap();
        assert_eq!(claimed.canonical_session_id.as_deref(), Some("child"));
    }

    #[test]
    fn checked_claim_reports_missing_group() {
        let dir = tempdir().unwrap();
        let err =
            claim_canonical_checked(dir.path(), "missing", "child", None, |_| true).unwrap_err();
        assert!(matches!(err, ClaimCanonicalError::GroupNotFound(_)));
    }

    #[test]
    fn old_ledger_json_without_new_fields_deserializes() {
        let dir = tempdir().unwrap();
        let gid = group_id("parent", "call-123");
        let old_json = format!(
            r#"{{
  "groups": [
    {{
      "group_id": "{gid}",
      "parent_session_id": "parent",
      "anchor_item_id": "call-123",
      "tool": "spawn_agent",
      "created_at": "2026-06-01T00:00:00Z",
      "updated_at": "2026-06-01T00:00:00Z",
      "branches": [
        {{
          "session_id": "child",
          "status": "running",
          "raw_log": "session.jsonl#session_id=child",
          "updated_at": "2026-06-01T00:00:00Z"
        }}
      ]
    }}
  ]
}}"#
        );
        fs::create_dir_all(dir.path()).unwrap();
        fs::write(ledger_path(dir.path()), old_json).unwrap();

        let document = read_fission_ledger_document(dir.path())
            .unwrap()
            .expect("document");
        assert!(document.ext.is_empty());
        assert_eq!(document.groups.len(), 1);
        assert_eq!(document.group_ext(&gid), None);
        assert_eq!(document.branch_ext(&gid, "child"), None);
        assert!(!document.group_is_detached(&gid));

        // The plain reader parses the same bytes.
        let ledger = read_fission_ledger(dir.path()).unwrap().expect("ledger");
        assert_eq!(ledger.groups, document.clone().into_ledger().groups);

        // Mutating through a new API keeps the file readable by the plain
        // reader: old readers simply ignore the unknown `ext` key.
        detach_group(dir.path(), &gid, "anchor gone").unwrap();
        let ledger = read_fission_ledger(dir.path()).unwrap().expect("ledger");
        assert_eq!(ledger.groups[0].branches[0].status, "detached");
    }

    #[test]
    fn empty_extension_keeps_old_wire_format() {
        let dir = tempdir().unwrap();
        record_fission_observation(dir.path(), observation("running")).unwrap();
        let raw = fs::read_to_string(ledger_path(dir.path())).unwrap();
        assert!(!raw.contains("\"ext\""));
        // Byte-identical to what the pre-extension serializer produced.
        let ledger = read_fission_ledger(dir.path()).unwrap().unwrap();
        assert_eq!(raw, serde_json::to_string_pretty(&ledger).unwrap());
    }

    #[test]
    fn document_serde_round_trips_extension_state() {
        let dir = tempdir().unwrap();
        record_fission_observation(dir.path(), observation("running")).unwrap();
        let gid = group_id("parent", "call-123");
        register_spawned_branch(
            dir.path(),
            "parent",
            "call-123",
            BranchCharter {
                objective: "trace the regression".to_string(),
                write_scope: Some("src/parser/".to_string()),
                worktree_requested: true,
            },
            NewSpawnedBranch {
                session_id: "spawned".to_string(),
                ..Default::default()
            },
        )
        .unwrap();
        update_branch_work(
            dir.path(),
            &gid,
            "spawned",
            &["src/parser/lex.rs".to_string()],
            &["cargo test --bin intendant parser".to_string()],
            Some("found it"),
        )
        .unwrap();
        mark_branch_imported(dir.path(), &gid, "spawned", None).unwrap();
        detach_group(dir.path(), &gid, "anchor rewound").unwrap();

        let document = read_fission_ledger_document(dir.path())
            .unwrap()
            .expect("document");
        let json = serde_json::to_string(&document).unwrap();
        let reparsed: FissionLedgerDocument = serde_json::from_str(&json).unwrap();
        assert_eq!(reparsed, document);

        let branch_ext = document.branch_ext(&gid, "spawned").expect("branch ext");
        assert_eq!(
            branch_ext.charter.as_ref().map(|c| c.objective.as_str()),
            Some("trace the regression")
        );
        assert!(branch_ext.imported_at.is_some());
        assert_eq!(branch_ext.changed_files, vec!["src/parser/lex.rs"]);
        assert_eq!(
            branch_ext.tests_run,
            vec!["cargo test --bin intendant parser"]
        );
    }

    #[test]
    fn charter_serde_omits_empty_optionals() {
        let json = serde_json::to_string(&BranchCharter {
            objective: "x".to_string(),
            write_scope: None,
            worktree_requested: false,
        })
        .unwrap();
        assert_eq!(json, r#"{"objective":"x"}"#);
        let parsed: BranchCharter = serde_json::from_str(r#"{"objective":"x"}"#).unwrap();
        assert_eq!(parsed.write_scope, None);
        assert!(!parsed.worktree_requested);
    }

    #[test]
    fn mark_branch_imported_records_marker_without_changing_detached_status() {
        let dir = tempdir().unwrap();
        record_fission_observation(dir.path(), observation("running")).unwrap();
        let gid = group_id("parent", "call-123");
        detach_group(dir.path(), &gid, "anchor rewound").unwrap();

        let group =
            mark_branch_imported(dir.path(), &gid, "child", Some("salvaged the diff")).unwrap();
        let branch = &group.branches[0];
        // Import is artifact-level: the branch stays detached.
        assert_eq!(branch.status, "detached");
        assert_eq!(branch.summary.as_deref(), Some("salvaged the diff"));

        let document = read_fission_ledger_document(dir.path()).unwrap().unwrap();
        assert!(document
            .branch_ext(&gid, "child")
            .expect("branch ext")
            .imported_at
            .is_some());
        assert!(document.group_is_detached(&gid));

        // Re-import refreshes the marker; a None summary keeps the prior one.
        mark_branch_imported(dir.path(), &gid, "child", None).unwrap();
        let document = read_fission_ledger_document(dir.path()).unwrap().unwrap();
        assert!(document
            .branch_ext(&gid, "child")
            .unwrap()
            .imported_at
            .is_some());
        let ledger = read_fission_ledger(dir.path()).unwrap().unwrap();
        assert_eq!(
            ledger.groups[0].branches[0].summary.as_deref(),
            Some("salvaged the diff")
        );
    }

    #[test]
    fn mark_branch_imported_requires_existing_group_and_branch() {
        let dir = tempdir().unwrap();
        let err = mark_branch_imported(dir.path(), "missing", "child", None).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
        record_fission_observation(dir.path(), observation("running")).unwrap();
        let gid = group_id("parent", "call-123");
        let err = mark_branch_imported(dir.path(), &gid, "missing-branch", None).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn update_branch_work_merges_first_seen_unique_lists() {
        let dir = tempdir().unwrap();
        record_fission_observation(dir.path(), observation("running")).unwrap();
        let gid = group_id("parent", "call-123");
        update_branch_work(
            dir.path(),
            &gid,
            "child",
            &["a.rs".to_string(), "b.rs".to_string()],
            &["cargo test a".to_string()],
            None,
        )
        .unwrap();
        // Cumulative re-report plus a delta: duplicates collapse, order stays
        // first-seen, blank entries are ignored, empty lists leave state alone.
        let group = update_branch_work(
            dir.path(),
            &gid,
            "child",
            &["b.rs".to_string(), "c.rs".to_string(), "  ".to_string()],
            &[],
            Some("progress"),
        )
        .unwrap();
        assert_eq!(group.branches[0].summary.as_deref(), Some("progress"));
        let document = read_fission_ledger_document(dir.path()).unwrap().unwrap();
        let branch_ext = document.branch_ext(&gid, "child").expect("branch ext");
        assert_eq!(branch_ext.changed_files, vec!["a.rs", "b.rs", "c.rs"]);
        assert_eq!(branch_ext.tests_run, vec!["cargo test a"]);

        let err = update_branch_work(dir.path(), &gid, "nope", &[], &[], None).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn register_spawned_branch_keys_group_like_observations_and_round_trips_charter() {
        let dir = tempdir().unwrap();
        let charter = BranchCharter {
            objective: "audit the encoder".to_string(),
            write_scope: Some("src/bin/caller/display/".to_string()),
            worktree_requested: true,
        };
        let group = register_spawned_branch(
            dir.path(),
            "parent",
            "call-123",
            charter.clone(),
            NewSpawnedBranch {
                session_id: "spawned".to_string(),
                backend_session_id: Some("thread-spawned".to_string()),
                worktree_path: Some(PathBuf::from("/tmp/wt")),
                task: Some("audit encoder paths".to_string()),
                model: Some("gpt-5.2-codex".to_string()),
                reasoning_effort: Some("high".to_string()),
                raw_log: None,
            },
        )
        .unwrap();

        assert_eq!(group.group_id, group_id("parent", "call-123"));
        assert_eq!(group.tool, "spawn_agent");
        assert_eq!(group.objective.as_deref(), Some("audit the encoder"));
        let branch = &group.branches[0];
        assert_eq!(branch.status, "running");
        assert_eq!(branch.backend_session_id.as_deref(), Some("thread-spawned"));
        assert_eq!(branch.task.as_deref(), Some("audit encoder paths"));
        assert_eq!(branch.worktree_path.as_deref(), Some(Path::new("/tmp/wt")));
        // The raw-log pointer falls back to the observed-branch convention.
        assert_eq!(branch.raw_log, "session.jsonl#session_id=spawned");

        // A sibling's later registration must not clobber the group charter
        // fields filled by the first writer.
        register_spawned_branch(
            dir.path(),
            "parent",
            "call-123",
            BranchCharter {
                objective: "second mandate".to_string(),
                write_scope: None,
                worktree_requested: false,
            },
            NewSpawnedBranch {
                session_id: "spawned-2".to_string(),
                ..Default::default()
            },
        )
        .unwrap();

        // A collab observation for the same (parent, anchor) merges into the
        // same group instead of creating a sibling group.
        let group = record_fission_observation(dir.path(), observation("running"))
            .unwrap()
            .expect("group");
        assert_eq!(group.group_id, group_id("parent", "call-123"));
        assert_eq!(group.branches.len(), 3);

        let document = read_fission_ledger_document(dir.path())
            .unwrap()
            .expect("document");
        assert_eq!(document.groups.len(), 1);
        assert_eq!(
            document
                .branch_ext(&group.group_id, "spawned")
                .expect("branch ext")
                .charter
                .as_ref(),
            Some(&charter)
        );
        assert_eq!(
            document
                .branch_ext(&group.group_id, "spawned-2")
                .expect("branch ext")
                .charter
                .as_ref()
                .map(|c| c.objective.as_str()),
            Some("second mandate")
        );
        // Observation-discovered branches carry no charter.
        assert_eq!(document.branch_ext(&group.group_id, "child"), None);
    }

    #[test]
    fn register_spawned_branch_validates_inputs() {
        let dir = tempdir().unwrap();
        let charter = BranchCharter {
            objective: "x".to_string(),
            write_scope: None,
            worktree_requested: false,
        };
        let branch = NewSpawnedBranch {
            session_id: "child".to_string(),
            ..Default::default()
        };
        let cases: Vec<(&str, &str, BranchCharter, NewSpawnedBranch)> = vec![
            ("", "call-1", charter.clone(), branch.clone()),
            ("parent", " ", charter.clone(), branch.clone()),
            (
                "parent",
                "call-1",
                charter.clone(),
                NewSpawnedBranch {
                    session_id: "  ".to_string(),
                    ..Default::default()
                },
            ),
            (
                "parent",
                "call-1",
                charter.clone(),
                NewSpawnedBranch {
                    session_id: "parent".to_string(),
                    ..Default::default()
                },
            ),
            (
                "parent",
                "call-1",
                BranchCharter {
                    objective: "  ".to_string(),
                    write_scope: None,
                    worktree_requested: false,
                },
                branch.clone(),
            ),
        ];
        for (parent, anchor, charter, branch) in cases {
            let err =
                register_spawned_branch(dir.path(), parent, anchor, charter, branch).unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        }
        // No partial writes happened.
        assert!(read_fission_ledger(dir.path()).unwrap().is_none());
    }

    #[test]
    fn register_spawned_branch_refuses_detached_group() {
        let dir = tempdir().unwrap();
        record_fission_observation(dir.path(), observation("running")).unwrap();
        let gid = group_id("parent", "call-123");
        detach_group(dir.path(), &gid, "anchor rewound").unwrap();
        let err = register_spawned_branch(
            dir.path(),
            "parent",
            "call-123",
            BranchCharter {
                objective: "too late".to_string(),
                write_scope: None,
                worktree_requested: false,
            },
            NewSpawnedBranch {
                session_id: "late".to_string(),
                ..Default::default()
            },
        )
        .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn register_spawned_branch_does_not_downgrade_terminal_branch() {
        let dir = tempdir().unwrap();
        record_fission_observation(dir.path(), observation("completed")).unwrap();
        let group = register_spawned_branch(
            dir.path(),
            "parent",
            "call-123",
            BranchCharter {
                objective: "retry".to_string(),
                write_scope: None,
                worktree_requested: false,
            },
            NewSpawnedBranch {
                session_id: "child".to_string(),
                model: Some("gpt-5.3-codex".to_string()),
                ..Default::default()
            },
        )
        .unwrap();
        let branch = &group.branches[0];
        // The terminal status is kept; the explicit metadata still lands.
        assert_eq!(branch.status, "completed");
        assert_eq!(branch.model.as_deref(), Some("gpt-5.3-codex"));
    }

    #[test]
    fn plain_ledger_persist_preserves_extension_state() {
        let dir = tempdir().unwrap();
        record_fission_observation(dir.path(), observation("running")).unwrap();
        let gid = group_id("parent", "call-123");
        detach_group(dir.path(), &gid, "anchor rewound").unwrap();

        // A legacy writer that only knows the plain ledger re-persists it...
        let ledger = read_fission_ledger(dir.path()).unwrap().unwrap();
        persist_fission_ledger(dir.path(), &ledger).unwrap();

        // ...and the extension state survives.
        let document = read_fission_ledger_document(dir.path()).unwrap().unwrap();
        assert!(document.group_is_detached(&gid));
        assert_eq!(
            document.group_ext(&gid).unwrap().detach_reason.as_deref(),
            Some("anchor rewound")
        );
    }

    #[test]
    fn session_document_view_follows_group_filter() {
        let dir = tempdir().unwrap();
        record_fission_observation(dir.path(), observation("running")).unwrap();
        let unrelated = FissionObservation {
            parent_session_id: "other-parent".to_string(),
            anchor_item_id: "other-call".to_string(),
            tool: "spawn_agent".to_string(),
            status: "running".to_string(),
            prompt: None,
            model: None,
            reasoning_effort: None,
            branches: vec![FissionBranchObservation {
                session_id: "other-child".to_string(),
                status: "running".to_string(),
                summary: None,
            }],
        };
        record_fission_observation(dir.path(), unrelated).unwrap();
        detach_group(dir.path(), &group_id("parent", "call-123"), "severed").unwrap();
        detach_group(
            dir.path(),
            &group_id("other-parent", "other-call"),
            "severed",
        )
        .unwrap();

        let document = read_fission_ledger_document_for_session(dir.path(), "child")
            .unwrap()
            .expect("document");
        assert_eq!(document.groups.len(), 1);
        assert_eq!(document.groups[0].parent_session_id, "parent");
        // Extension entries follow their groups through the filter.
        assert_eq!(document.ext.groups.len(), 1);
        assert!(document.group_is_detached(&group_id("parent", "call-123")));
    }
}
