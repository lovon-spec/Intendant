use serde::{Deserialize, Serialize};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ContextRewindRecord {
    pub record_id: String,
    pub created_at: String,
    pub session_id: Option<String>,
    pub thread_id: String,
    pub item_id: String,
    pub position: String,
    pub reason: Option<String>,
    pub primer: Option<String>,
    pub preserve: Vec<String>,
    pub discard: Vec<String>,
    pub artifacts: Vec<String>,
    pub next_steps: Vec<String>,
    pub source_rollout_path: Option<PathBuf>,
    pub recovery_rollout_path: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fission_snapshot: Option<FissionSnapshot>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lineage_ledger: Option<crate::lineage_ledger::LineageLedger>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fission_ledger: Option<crate::fission_ledger::FissionLedger>,
    /// Fission groups whose spawn anchors this rewind cut out of the
    /// effective history (severed by `detach_groups_with_invalid_anchors`
    /// right after the rollback). Backward compatible: records written before
    /// the field existed deserialize as empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub detached_fission_group_ids: Vec<String>,
    /// Backend-reported tokens in the context when this record was created,
    /// from the freshest usage snapshot locally available at that moment (no
    /// extra backend RPC): the pre-rewind rollout's last `token_count`
    /// report, else the latest persisted session-log context snapshot.
    /// `None` when neither carried a backend-reported count. Backward
    /// compatible: records written before the field existed deserialize as
    /// `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub used_tokens_at_rewind: Option<u64>,
    /// Effective context window (the rewind-only limit) paired with
    /// `used_tokens_at_rewind`, from the same snapshot. Backward compatible:
    /// `None` on older records.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window_at_rewind: Option<u64>,
    /// Pressure band at record creation: `"ok"`, `"watch"` (at or above the
    /// managed-context density threshold share of the window), `"high"` (at
    /// or above the window), or `"critical"` (at or above the hard window,
    /// when the snapshot knew it). Derived from the two fields above by the
    /// record writer — thresholds mirror the live managed-context gates.
    /// Backward compatible: `None` on older records.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pressure_band_at_rewind: Option<String>,
    /// True when the supervisor itself chose the anchor and authored the
    /// primer (surgical recovery after the model exhausted its recovery
    /// step limit without rewinding), as opposed to a model-authored
    /// `rewind_context` call. Backward compatible: records written before
    /// the field existed deserialize as `false` and the key is omitted for
    /// ordinary model rewinds.
    #[serde(default, skip_serializing_if = "is_false")]
    pub surgical: bool,
}

fn is_false(value: &bool) -> bool {
    !*value
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FissionSnapshot {
    pub source_session_id: String,
    pub identities: Vec<FissionSessionIdentity>,
    pub relationships: Vec<FissionRelationship>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FissionSessionIdentity {
    pub session_id: String,
    pub source: String,
    pub backend_session_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FissionRelationship {
    pub parent_session_id: String,
    pub child_session_id: String,
    pub relationship: String,
    pub ephemeral: bool,
}

pub fn records_dir(log_dir: &Path) -> PathBuf {
    log_dir.join("context_rewinds")
}

pub fn record_path(log_dir: &Path, record_id: &str) -> PathBuf {
    records_dir(log_dir).join(format!("{record_id}.json"))
}

pub fn recovery_rollout_path(log_dir: &Path, record_id: &str) -> PathBuf {
    records_dir(log_dir).join(format!("{record_id}-source-rollout.jsonl"))
}

pub fn persist_record(log_dir: &Path, record: &ContextRewindRecord) -> io::Result<()> {
    let dir = records_dir(log_dir);
    fs::create_dir_all(&dir)?;
    let bytes = serde_json::to_vec_pretty(record).map_err(io::Error::other)?;
    // Atomic write so a crash mid-write can't leave a truncated record that
    // `list_records` would silently skip (defeating durable recovery).
    crate::file_watcher::atomic_write(&record_path(log_dir, &record.record_id), &bytes)
}

pub fn read_record(log_dir: &Path, record_id: &str) -> io::Result<ContextRewindRecord> {
    let bytes = fs::read(record_path(log_dir, record_id))?;
    serde_json::from_slice(&bytes).map_err(io::Error::other)
}

pub fn list_records(log_dir: &Path) -> io::Result<Vec<ContextRewindRecord>> {
    let dir = records_dir(log_dir);
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err),
    };
    let mut records: Vec<ContextRewindRecord> = Vec::new();
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }
        match fs::read(&path)
            .and_then(|bytes| serde_json::from_slice(&bytes).map_err(io::Error::other))
        {
            Ok(record) => records.push(record),
            Err(_) => continue,
        }
    }
    records.sort_by(|a, b| b.created_at.cmp(&a.created_at));
    Ok(records)
}

pub fn copy_recovery_rollout(
    log_dir: &Path,
    record_id: &str,
    source_rollout_path: &Path,
) -> io::Result<PathBuf> {
    let target = recovery_rollout_path(log_dir, record_id);
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::copy(source_rollout_path, &target)?;
    Ok(target)
}

/// Delete a recovery-rollout copy. Used to clean up the copy-before-mutation
/// artifact when a rewind's rollback fails, so a failed rewind leaves no
/// orphaned files behind. A missing file is not an error.
pub fn remove_recovery_rollout(log_dir: &Path, record_id: &str) -> io::Result<()> {
    match fs::remove_file(recovery_rollout_path(log_dir, record_id)) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err),
    }
}

pub fn read_fission_snapshot(
    log_dir: &Path,
    source_session_id: &str,
) -> io::Result<Option<FissionSnapshot>> {
    let path = log_dir.join("session.jsonl");
    let contents = match fs::read_to_string(&path) {
        Ok(contents) => contents,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err),
    };
    let mut identities = Vec::new();
    let mut relationships = Vec::new();
    for line in contents.lines() {
        let Ok(entry) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        match entry.get("event").and_then(|event| event.as_str()) {
            Some("session_identity") => {
                let Some(data) = entry.get("data") else {
                    continue;
                };
                let identity = FissionSessionIdentity {
                    session_id: json_string(data, "session_id"),
                    source: json_string(data, "source"),
                    backend_session_id: json_string(data, "backend_session_id"),
                };
                if !identity.session_id.is_empty()
                    && !identity.source.is_empty()
                    && !identity.backend_session_id.is_empty()
                    && !identities.contains(&identity)
                {
                    identities.push(identity);
                }
            }
            Some("session_relationship") => {
                let Some(data) = entry.get("data") else {
                    continue;
                };
                let relationship = FissionRelationship {
                    parent_session_id: json_string(data, "parent_session_id"),
                    child_session_id: json_string(data, "child_session_id"),
                    relationship: json_string(data, "relationship"),
                    ephemeral: data
                        .get("ephemeral")
                        .and_then(|value| value.as_bool())
                        .unwrap_or(false),
                };
                if !relationship.parent_session_id.is_empty()
                    && !relationship.child_session_id.is_empty()
                    && !relationship.relationship.is_empty()
                    && !relationships.contains(&relationship)
                {
                    relationships.push(relationship);
                }
            }
            _ => {}
        }
    }
    if identities.is_empty() && relationships.is_empty() {
        return Ok(None);
    }
    Ok(Some(FissionSnapshot {
        source_session_id: source_session_id.to_string(),
        identities,
        relationships,
    }))
}

fn json_string(data: &serde_json::Value, key: &str) -> String {
    data.get(key)
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or_default()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn minimal_record(
        record_id: &str,
        created_at: &str,
        session_id: Option<&str>,
        thread_id: &str,
    ) -> ContextRewindRecord {
        ContextRewindRecord {
            record_id: record_id.to_string(),
            created_at: created_at.to_string(),
            session_id: session_id.map(str::to_string),
            thread_id: thread_id.to_string(),
            item_id: "call-1".to_string(),
            position: "after".to_string(),
            reason: Some("trim noisy output".to_string()),
            primer: Some("keep this".to_string()),
            preserve: vec!["fact".to_string()],
            discard: vec!["noise".to_string()],
            artifacts: vec!["log.txt".to_string()],
            next_steps: vec!["continue".to_string()],
            source_rollout_path: None,
            recovery_rollout_path: None,
            fission_snapshot: None,
            lineage_ledger: None,
            fission_ledger: None,
            detached_fission_group_ids: Vec::new(),
            used_tokens_at_rewind: None,
            context_window_at_rewind: None,
            pressure_band_at_rewind: None,
            surgical: false,
        }
    }

    #[test]
    fn persist_and_read_context_rewind_record() {
        let dir = tempdir().unwrap();
        let record = ContextRewindRecord {
            record_id: "rewind-1".to_string(),
            created_at: "2026-05-25T00:00:00Z".to_string(),
            session_id: Some("session-1".to_string()),
            thread_id: "thread-1".to_string(),
            item_id: "call-1".to_string(),
            position: "after".to_string(),
            reason: Some("trim noisy output".to_string()),
            primer: Some("keep this".to_string()),
            preserve: vec!["fact".to_string()],
            discard: vec!["noise".to_string()],
            artifacts: vec!["log.txt".to_string()],
            next_steps: vec!["continue".to_string()],
            source_rollout_path: Some(PathBuf::from("/tmp/source.jsonl")),
            recovery_rollout_path: Some(PathBuf::from("/tmp/recovery.jsonl")),
            fission_snapshot: Some(FissionSnapshot {
                source_session_id: "thread-1".to_string(),
                identities: vec![FissionSessionIdentity {
                    session_id: "thread-1".to_string(),
                    source: "codex".to_string(),
                    backend_session_id: "thread-1".to_string(),
                }],
                relationships: vec![FissionRelationship {
                    parent_session_id: "thread-1".to_string(),
                    child_session_id: "thread-child".to_string(),
                    relationship: "subagent".to_string(),
                    ephemeral: false,
                }],
            }),
            lineage_ledger: Some(crate::lineage_ledger::LineageLedger {
                source_session_id: "thread-1".to_string(),
                groups: vec![crate::lineage_ledger::LineageGroup {
                    group_id: "session:thread-1".to_string(),
                    parent_session_id: "thread-1".to_string(),
                    canonical_session_id: Some("thread-1".to_string()),
                    branches: vec![crate::lineage_ledger::LineageBranch {
                        session_id: "thread-child".to_string(),
                        backend_session_id: Some("thread-child".to_string()),
                        relationship: "subagent".to_string(),
                        status: "running".to_string(),
                        task: None,
                        summary: None,
                        raw_log: "session.jsonl#session_id=thread-child".to_string(),
                        ephemeral: false,
                    }],
                }],
            }),
            fission_ledger: Some(crate::fission_ledger::FissionLedger {
                groups: vec![crate::fission_ledger::FissionGroup {
                    group_id: "fission-thread-1-call-1".to_string(),
                    parent_session_id: "thread-1".to_string(),
                    anchor_item_id: "call-1".to_string(),
                    tool: "spawn_agent".to_string(),
                    objective: Some("inspect".to_string()),
                    prompt: Some("inspect".to_string()),
                    created_at: "2026-05-25T00:00:00Z".to_string(),
                    updated_at: "2026-05-25T00:00:00Z".to_string(),
                    canonical_session_id: Some("thread-child".to_string()),
                    branches: vec![crate::fission_ledger::FissionBranch {
                        session_id: "thread-child".to_string(),
                        backend_session_id: Some("thread-child".to_string()),
                        status: "completed".to_string(),
                        summary: Some("done".to_string()),
                        task: Some("inspect".to_string()),
                        model: None,
                        reasoning_effort: None,
                        worktree_path: None,
                        raw_log: "session.jsonl#session_id=thread-child".to_string(),
                        ephemeral: false,
                        updated_at: "2026-05-25T00:00:00Z".to_string(),
                    }],
                }],
            }),
            detached_fission_group_ids: vec!["fission-thread-1-call-1".to_string()],
            used_tokens_at_rewind: Some(36_500),
            context_window_at_rewind: Some(38_000),
            pressure_band_at_rewind: Some("watch".to_string()),
            surgical: false,
        };

        persist_record(dir.path(), &record).unwrap();
        assert_eq!(read_record(dir.path(), "rewind-1").unwrap(), record);
    }

    #[test]
    fn record_without_detached_fission_groups_round_trips_compactly() {
        // Old records (and new records that detached nothing) carry no
        // `detached_fission_group_ids` key at all; they must deserialize as
        // an empty list and serialize back without the key. The same compact
        // contract holds for the optional pressure-at-rewind fields.
        let dir = tempdir().unwrap();
        let record = minimal_record("rewind-2", "2026-05-25T00:00:00Z", None, "thread-2");
        assert!(record.detached_fission_group_ids.is_empty());

        persist_record(dir.path(), &record).unwrap();
        let raw = fs::read_to_string(record_path(dir.path(), "rewind-2")).unwrap();
        assert!(!raw.contains("detached_fission_group_ids"));
        assert!(!raw.contains("used_tokens_at_rewind"));
        assert!(!raw.contains("context_window_at_rewind"));
        assert!(!raw.contains("pressure_band_at_rewind"));
        assert!(!raw.contains("surgical"));

        let read = read_record(dir.path(), "rewind-2").unwrap();
        assert_eq!(read, record);
        assert!(read.detached_fission_group_ids.is_empty());
        assert!(read.used_tokens_at_rewind.is_none());
        assert!(read.context_window_at_rewind.is_none());
        assert!(read.pressure_band_at_rewind.is_none());
        assert!(!read.surgical);
    }

    #[test]
    fn surgical_record_round_trips_and_legacy_records_deserialize_as_model_rewinds() {
        let dir = tempdir().unwrap();
        let mut record = minimal_record("rewind-surgical", "2026-06-12T00:00:00Z", None, "t-1");
        record.surgical = true;

        persist_record(dir.path(), &record).unwrap();
        let raw = fs::read_to_string(record_path(dir.path(), "rewind-surgical")).unwrap();
        assert!(raw.contains("\"surgical\": true"));
        assert_eq!(read_record(dir.path(), "rewind-surgical").unwrap(), record);

        // Records written before the field existed carry no `surgical` key
        // and must deserialize as ordinary model rewinds.
        let legacy = serde_json::json!({
            "record_id": "rewind-legacy-3",
            "created_at": "2026-06-12T00:00:00Z",
            "session_id": null,
            "thread_id": "t-1",
            "item_id": "call-1",
            "position": "after",
            "reason": "trim noisy output",
            "primer": "keep this",
            "preserve": [],
            "discard": [],
            "artifacts": [],
            "next_steps": [],
            "source_rollout_path": null,
            "recovery_rollout_path": null,
        });
        let legacy: ContextRewindRecord = serde_json::from_value(legacy).unwrap();
        assert!(!legacy.surgical);
    }

    #[test]
    fn record_with_pressure_at_rewind_round_trips() {
        let dir = tempdir().unwrap();
        let mut record = minimal_record("rewind-3", "2026-06-12T00:00:00Z", None, "thread-3");
        record.used_tokens_at_rewind = Some(41_772);
        record.context_window_at_rewind = Some(38_000);
        record.pressure_band_at_rewind = Some("high".to_string());

        persist_record(dir.path(), &record).unwrap();
        let raw = fs::read_to_string(record_path(dir.path(), "rewind-3")).unwrap();
        assert!(raw.contains("used_tokens_at_rewind"));
        assert!(raw.contains("context_window_at_rewind"));
        assert!(raw.contains("pressure_band_at_rewind"));
        assert_eq!(read_record(dir.path(), "rewind-3").unwrap(), record);
    }

    #[test]
    fn legacy_record_json_without_detach_field_deserializes() {
        // A record persisted by a pre-fission build: no
        // `detached_fission_group_ids` (and no pressure-at-rewind) key
        // anywhere.
        let legacy = serde_json::json!({
            "record_id": "rewind-legacy",
            "created_at": "2026-05-25T00:00:00Z",
            "session_id": "session-1",
            "thread_id": "thread-1",
            "item_id": "call-1",
            "position": "after",
            "reason": "trim noisy output",
            "primer": "keep this",
            "preserve": [],
            "discard": [],
            "artifacts": [],
            "next_steps": [],
            "source_rollout_path": null,
            "recovery_rollout_path": null,
        });
        let record: ContextRewindRecord = serde_json::from_value(legacy).unwrap();
        assert!(record.detached_fission_group_ids.is_empty());
        assert!(record.used_tokens_at_rewind.is_none());
        assert!(record.context_window_at_rewind.is_none());
        assert!(record.pressure_band_at_rewind.is_none());
    }

    #[test]
    fn legacy_record_json_without_pressure_fields_deserializes() {
        // A record persisted by a post-fission but pre-pressure build (e.g.
        // the 2026-06-12 constrained-window bench pilot): it may carry
        // `detached_fission_group_ids` but none of the pressure fields.
        let legacy = serde_json::json!({
            "record_id": "rewind-legacy-2",
            "created_at": "2026-06-12T06:11:01.415500873+00:00",
            "session_id": "intendant",
            "thread_id": "thread-1",
            "item_id": "call-1",
            "position": "after",
            "reason": "trim noisy output",
            "primer": "keep this",
            "preserve": ["fact"],
            "discard": ["noise"],
            "artifacts": [],
            "next_steps": [],
            "source_rollout_path": "/tmp/source.jsonl",
            "recovery_rollout_path": "/tmp/recovery.jsonl",
            "detached_fission_group_ids": ["fission-thread-1-call-1"],
        });
        let record: ContextRewindRecord = serde_json::from_value(legacy).unwrap();
        assert_eq!(
            record.detached_fission_group_ids,
            vec!["fission-thread-1-call-1".to_string()]
        );
        assert!(record.used_tokens_at_rewind.is_none());
        assert!(record.context_window_at_rewind.is_none());
        assert!(record.pressure_band_at_rewind.is_none());
    }

    #[test]
    fn list_records_returns_newest_first_and_skips_invalid_entries() {
        let dir = tempdir().unwrap();
        persist_record(
            dir.path(),
            &minimal_record(
                "older",
                "2026-05-25T00:00:00Z",
                Some("session-a"),
                "thread-a",
            ),
        )
        .unwrap();
        persist_record(
            dir.path(),
            &minimal_record(
                "newer",
                "2026-05-26T00:00:00Z",
                Some("session-a"),
                "thread-a",
            ),
        )
        .unwrap();
        fs::write(records_dir(dir.path()).join("invalid.json"), "{not json").unwrap();
        fs::write(records_dir(dir.path()).join("ignored.txt"), "{}").unwrap();

        let records = list_records(dir.path()).unwrap();
        let ids: Vec<_> = records
            .iter()
            .map(|record| record.record_id.as_str())
            .collect();
        assert_eq!(ids, vec!["newer", "older"]);
    }

    #[test]
    fn list_records_returns_empty_when_records_dir_is_missing() {
        let dir = tempdir().unwrap();
        assert!(list_records(dir.path()).unwrap().is_empty());
    }

    #[test]
    fn copies_recovery_rollout_before_mutation() {
        let dir = tempdir().unwrap();
        let source = dir.path().join("source.jsonl");
        fs::write(&source, "pre-rewind-history\n").unwrap();

        let copied = copy_recovery_rollout(dir.path(), "rewind-1", &source).unwrap();

        fs::write(&source, "mutated\n").unwrap();
        assert_eq!(fs::read_to_string(copied).unwrap(), "pre-rewind-history\n");
    }

    #[test]
    fn reads_fission_snapshot_from_session_log() {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("session.jsonl"),
            concat!(
                r#"{"event":"session_identity","data":{"session_id":"parent","source":"codex","backend_session_id":"thread-parent"}}"#,
                "\n",
                r#"{"event":"session_relationship","data":{"parent_session_id":"parent","child_session_id":"child","relationship":"subagent","ephemeral":false}}"#,
                "\n",
            ),
        )
        .unwrap();

        let snapshot = read_fission_snapshot(dir.path(), "thread-parent")
            .unwrap()
            .expect("snapshot");
        assert_eq!(snapshot.source_session_id, "thread-parent");
        assert_eq!(snapshot.identities.len(), 1);
        assert_eq!(snapshot.relationships.len(), 1);
        assert_eq!(snapshot.relationships[0].relationship, "subagent");
    }
}
