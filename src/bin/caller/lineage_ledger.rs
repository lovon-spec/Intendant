use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::io;
use std::path::Path;

/// Boilerplate the session log writes for a `done_signal` with no caller message
/// (see `SessionLog::done_signal_for_session`). Filtered out so it isn't treated
/// as a model-authored branch summary.
const DONE_SIGNAL_DEFAULT_MESSAGE: &str = "Agent signalled done";

/// Parent/child session relationships for one session's connected component,
/// derived from `session.jsonl` (never persisted as its own file). Consumed
/// by the MCP `get_status` surface (`mcp.rs`) and embedded into rewind-record
/// snapshots (`main.rs` / `context_rewind.rs`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LineageLedger {
    pub source_session_id: String,
    pub groups: Vec<LineageGroup>,
}

/// All recorded child edges of one parent session.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LineageGroup {
    pub group_id: String,
    pub parent_session_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub canonical_session_id: Option<String>,
    pub branches: Vec<LineageBranch>,
}

/// One parent→child relationship row (see [`lineage_ledger_from_jsonl`] for
/// the recognized relationship kinds and their status conventions).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LineageBranch {
    pub session_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backend_session_id: Option<String>,
    pub relationship: String,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    pub raw_log: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub ephemeral: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct RelationshipKey {
    parent_session_id: String,
    child_session_id: String,
    relationship: String,
    ephemeral: bool,
}

#[derive(Default)]
struct SessionFacts {
    identities: HashMap<String, String>,
    tasks: HashMap<String, String>,
    summaries: HashMap<String, String>,
    statuses: HashMap<String, String>,
    relationships: BTreeSet<RelationshipKey>,
    /// Emission order (sequence index) of each relationship, so canonical-head
    /// selection can pick the *latest* rewind-restore rather than relying on the
    /// `BTreeSet`'s lexicographic ordering of (random) child session ids.
    relationship_order: HashMap<RelationshipKey, usize>,
    /// `(parent, child)` pairs severed by a `fission-detached` relationship:
    /// the branch's spawn anchor left the effective history (rewound past) or
    /// the group was explicitly severed.
    fission_detached: BTreeSet<(String, String)>,
    /// `(parent, child)` pairs whose result a `fission-imported` relationship
    /// marked as explicitly imported into the parent's continuation.
    fission_imported: BTreeSet<(String, String)>,
}

/// Read `session.jsonl` from `log_dir` and derive the lineage ledger for
/// `source_session_id` (see [`lineage_ledger_from_jsonl`]). Called by the MCP
/// `get_status` surface (`mcp.rs`) and the rewind-record snapshot path
/// (`main.rs`). `Ok(None)` when no session log exists.
pub fn read_lineage_ledger(
    log_dir: &Path,
    source_session_id: &str,
) -> io::Result<Option<LineageLedger>> {
    let path = log_dir.join("session.jsonl");
    let contents = match fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err),
    };
    Ok(lineage_ledger_from_jsonl(&contents, source_session_id))
}

/// Derive the lineage ledger for `source_session_id`'s connected component
/// from raw `session.jsonl` contents. Called by [`read_lineage_ledger`] (the
/// dashboard Managed tab / MCP `get_status` read side and the rewind path's
/// lineage snapshot in `main.rs`).
///
/// Branch rows come from `session_relationship` events. Specially handled
/// relationship kinds:
/// - `rewind-restore` — row status `restored`; the latest one becomes the
///   group's canonical head;
/// - `rewind-backout` — row status `inspection`;
/// - `fission-branch` — a fission spawn edge (written by the
///   `register_spawned_branch` / observation wiring); status follows the
///   child's observed lifecycle unless a fission marker overrides it;
/// - `fission-detached` / `fission-imported` — markers that update the spawn
///   row's status (`detached` / `imported`) instead of duplicating the row;
///   they only become rows of their own when the log carries no matching
///   spawn row (see [`fission_status_override`] for the precedence rules).
///
/// Everything else (`subagent`, `managed-edit-branch`, …) renders generically
/// with the child's observed status.
pub fn lineage_ledger_from_jsonl(contents: &str, source_session_id: &str) -> Option<LineageLedger> {
    let mut facts = SessionFacts::default();
    let mut relationship_seq = 0usize;
    let mut pending_fission_marks: Vec<RelationshipKey> = Vec::new();
    for line in contents.lines() {
        let Ok(entry) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let event = entry
            .get("event")
            .and_then(|value| value.as_str())
            .unwrap_or_default();
        let data = entry.get("data").unwrap_or(&serde_json::Value::Null);
        match event {
            "session_identity" => {
                let session_id = json_string(data, "session_id");
                let backend_session_id = json_string(data, "backend_session_id");
                if !session_id.is_empty() && !backend_session_id.is_empty() {
                    facts.identities.insert(session_id, backend_session_id);
                }
            }
            "session_started" => {
                let session_id = json_string(data, "session_id");
                let task = json_string(data, "task");
                if !session_id.is_empty() && !task.is_empty() {
                    facts.tasks.insert(session_id, task);
                }
            }
            "session_relationship" => {
                let rel = RelationshipKey {
                    parent_session_id: json_string(data, "parent_session_id"),
                    child_session_id: json_string(data, "child_session_id"),
                    relationship: json_string(data, "relationship"),
                    ephemeral: data
                        .get("ephemeral")
                        .and_then(|value| value.as_bool())
                        .unwrap_or(false),
                };
                if !rel.parent_session_id.is_empty()
                    && !rel.child_session_id.is_empty()
                    && !rel.relationship.is_empty()
                {
                    facts
                        .relationship_order
                        .insert(rel.clone(), relationship_seq);
                    relationship_seq += 1;
                    match rel.relationship.as_str() {
                        // Fission detach/import markers prefer updating their
                        // spawn row over becoming rows of their own; whether a
                        // spawn row exists is only known once the whole log
                        // has been scanned, so they are resolved after the
                        // loop (event order does not matter).
                        "fission-detached" | "fission-imported" => {
                            let pair =
                                (rel.parent_session_id.clone(), rel.child_session_id.clone());
                            if rel.relationship == "fission-detached" {
                                facts.fission_detached.insert(pair);
                            } else {
                                facts.fission_imported.insert(pair);
                            }
                            pending_fission_marks.push(rel);
                        }
                        _ => {
                            facts.relationships.insert(rel);
                        }
                    }
                }
            }
            "done_signal" => {
                let session_id = json_string(data, "session_id");
                if !session_id.is_empty() {
                    facts
                        .statuses
                        .insert(session_id.clone(), "completed".into());
                    // Ignore the writer's boilerplate default ("Agent signalled
                    // done") so it doesn't masquerade as a model-authored summary.
                    if let Some(message) = entry
                        .get("message")
                        .and_then(|value| value.as_str())
                        .map(str::trim)
                        .filter(|message| {
                            !message.is_empty() && *message != DONE_SIGNAL_DEFAULT_MESSAGE
                        })
                        .map(trim_summary)
                    {
                        facts.summaries.insert(session_id, message);
                    }
                }
            }
            "task_complete" => {
                let session_id = json_string(data, "session_id");
                if !session_id.is_empty() {
                    facts
                        .statuses
                        .insert(session_id.clone(), "completed".into());
                    let summary = data
                        .get("summary")
                        .and_then(|value| value.as_str())
                        .or_else(|| data.get("reason").and_then(|value| value.as_str()))
                        .map(trim_summary);
                    if let Some(summary) = summary {
                        facts.summaries.insert(session_id, summary);
                    }
                }
            }
            "session_ended" => {
                let session_id = json_string(data, "session_id");
                if !session_id.is_empty() {
                    // A generic teardown must not downgrade a completed task or
                    // clobber a model-authored summary with a terse reason.
                    if facts.statuses.get(&session_id).map(String::as_str) != Some("completed") {
                        facts.statuses.insert(session_id.clone(), "ended".into());
                    }
                    let reason = json_string(data, "reason");
                    if !reason.is_empty() && !facts.summaries.contains_key(&session_id) {
                        facts.summaries.insert(session_id, trim_summary(&reason));
                    }
                }
            }
            _ => {}
        }
    }

    // A detach/import marker dedups into its spawn row (`fission-branch`)
    // when one exists — the marker then only drives that row's status — and
    // becomes a standalone row otherwise (e.g. a truncated log that no longer
    // carries the spawn event), so the fact stays visible either way.
    for rel in pending_fission_marks {
        let has_spawn_row = facts.relationships.iter().any(|existing| {
            existing.relationship == "fission-branch"
                && existing.parent_session_id == rel.parent_session_id
                && existing.child_session_id == rel.child_session_id
        });
        if !has_spawn_row {
            facts.relationships.insert(rel);
        }
    }

    if facts.relationships.is_empty() {
        return None;
    }

    let relationships = related_relationships(facts.relationships, source_session_id);
    if relationships.is_empty() {
        return None;
    }

    let mut by_parent: BTreeMap<String, Vec<RelationshipKey>> = BTreeMap::new();
    for rel in relationships {
        by_parent
            .entry(rel.parent_session_id.clone())
            .or_default()
            .push(rel);
    }

    let mut groups = Vec::new();
    for (parent_session_id, relationships) in by_parent {
        let canonical_session_id = relationships
            .iter()
            .filter(|rel| rel.relationship == "rewind-restore")
            .max_by_key(|rel| facts.relationship_order.get(*rel).copied().unwrap_or(0))
            .map(|rel| rel.child_session_id.clone())
            .or_else(|| Some(parent_session_id.clone()));
        let branches = relationships
            .into_iter()
            .map(|rel| {
                let status = if rel.relationship == "rewind-restore" {
                    "restored".to_string()
                } else if rel.relationship == "rewind-backout" {
                    "inspection".to_string()
                } else if let Some(status) =
                    fission_status_override(&facts.fission_detached, &facts.fission_imported, &rel)
                {
                    status
                } else {
                    facts
                        .statuses
                        .get(&rel.child_session_id)
                        .cloned()
                        .unwrap_or_else(|| "running".to_string())
                };
                LineageBranch {
                    backend_session_id: facts.identities.get(&rel.child_session_id).cloned(),
                    task: facts.tasks.get(&rel.child_session_id).cloned(),
                    summary: facts.summaries.get(&rel.child_session_id).cloned(),
                    raw_log: format!("session.jsonl#session_id={}", rel.child_session_id),
                    session_id: rel.child_session_id,
                    relationship: rel.relationship,
                    status,
                    ephemeral: rel.ephemeral,
                }
            })
            .collect();
        groups.push(LineageGroup {
            group_id: format!("session:{parent_session_id}"),
            parent_session_id,
            canonical_session_id,
            branches,
        });
    }
    Some(LineageLedger {
        source_session_id: source_session_id.to_string(),
        groups,
    })
}

/// Status override for fission relationship rows (`fission-branch` /
/// `fission-detached` / `fission-imported`). Precedence mirrors the fission
/// ledger's stickiness rules — `detached` beats `imported` beats the child's
/// observed lifecycle status — so a detach survives both stray completion
/// events from a still-running child and artifact-level imports, and an
/// import is not downgraded by a later generic teardown event. Returns `None`
/// for non-fission rows (markers are scoped to fission edges; a `subagent`
/// edge for the same pair keeps its own lifecycle) and for plain spawn rows
/// without marks, which fall through to the observed status.
fn fission_status_override(
    fission_detached: &BTreeSet<(String, String)>,
    fission_imported: &BTreeSet<(String, String)>,
    rel: &RelationshipKey,
) -> Option<String> {
    if !matches!(
        rel.relationship.as_str(),
        "fission-branch" | "fission-detached" | "fission-imported"
    ) {
        return None;
    }
    let marked = |marks: &BTreeSet<(String, String)>| {
        marks.iter().any(|(parent, child)| {
            parent == &rel.parent_session_id && child == &rel.child_session_id
        })
    };
    if rel.relationship == "fission-detached" || marked(fission_detached) {
        return Some("detached".to_string());
    }
    if rel.relationship == "fission-imported" || marked(fission_imported) {
        return Some("imported".to_string());
    }
    None
}

fn related_relationships(
    relationships: BTreeSet<RelationshipKey>,
    source_session_id: &str,
) -> Vec<RelationshipKey> {
    // An empty source id has no lineage to anchor to; returning *all* relationships
    // would leak unrelated sessions' lineage. Callers always pass a concrete id.
    if source_session_id.trim().is_empty() {
        return Vec::new();
    }

    let mut related: BTreeSet<String> = [source_session_id.to_string()].into_iter().collect();
    loop {
        let before = related.len();
        for rel in &relationships {
            if related.contains(&rel.parent_session_id) || related.contains(&rel.child_session_id) {
                related.insert(rel.parent_session_id.clone());
                related.insert(rel.child_session_id.clone());
            }
        }
        if related.len() == before {
            break;
        }
    }

    relationships
        .into_iter()
        .filter(|rel| {
            related.contains(&rel.parent_session_id) || related.contains(&rel.child_session_id)
        })
        .collect()
}

fn json_string(data: &serde_json::Value, key: &str) -> String {
    data.get(key)
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or_default()
        .to_string()
}

fn trim_summary(value: &str) -> String {
    const MAX_CHARS: usize = 240;
    let value = value.trim();
    if value.chars().count() <= MAX_CHARS {
        return value.to_string();
    }
    let mut out: String = value.chars().take(MAX_CHARS).collect();
    out.push_str("...");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lineage_ledger_groups_relationships_by_parent() {
        let jsonl = concat!(
            r#"{"event":"session_identity","data":{"session_id":"child","source":"codex","backend_session_id":"thread-child"}}"#,
            "\n",
            r#"{"event":"session_started","data":{"session_id":"child","task":"check the parser"}}"#,
            "\n",
            r#"{"event":"session_relationship","data":{"parent_session_id":"parent","child_session_id":"child","relationship":"subagent","ephemeral":false}}"#,
            "\n",
            r#"{"event":"task_complete","data":{"session_id":"child","reason":"done","summary":"parser is fine"}}"#,
            "\n",
        );

        let ledger = lineage_ledger_from_jsonl(jsonl, "parent").expect("ledger");
        assert_eq!(ledger.groups.len(), 1);
        assert_eq!(ledger.groups[0].parent_session_id, "parent");
        assert_eq!(
            ledger.groups[0].canonical_session_id.as_deref(),
            Some("parent")
        );
        let branch = &ledger.groups[0].branches[0];
        assert_eq!(branch.session_id, "child");
        assert_eq!(branch.backend_session_id.as_deref(), Some("thread-child"));
        assert_eq!(branch.relationship, "subagent");
        assert_eq!(branch.status, "completed");
        assert_eq!(branch.task.as_deref(), Some("check the parser"));
        assert_eq!(branch.summary.as_deref(), Some("parser is fine"));
    }

    #[test]
    fn lineage_ledger_marks_rewind_restore_as_canonical() {
        let jsonl = concat!(
            r#"{"event":"session_relationship","data":{"parent_session_id":"old","child_session_id":"inspect","relationship":"rewind-backout","ephemeral":false}}"#,
            "\n",
            r#"{"event":"session_relationship","data":{"parent_session_id":"old","child_session_id":"restore","relationship":"rewind-restore","ephemeral":false}}"#,
            "\n",
        );

        let ledger = lineage_ledger_from_jsonl(jsonl, "old").expect("ledger");
        assert_eq!(
            ledger.groups[0].canonical_session_id.as_deref(),
            Some("restore")
        );
        assert_eq!(ledger.groups[0].branches[0].status, "inspection");
        assert_eq!(ledger.groups[0].branches[1].status, "restored");
    }

    #[test]
    fn lineage_ledger_canonical_is_latest_restore_not_lexicographic() {
        // Two restores against the same thread; the second-emitted ("aaa") is the
        // latest and must be canonical even though "zzz" sorts later lexically.
        let jsonl = concat!(
            r#"{"event":"session_relationship","data":{"parent_session_id":"old","child_session_id":"zzz","relationship":"rewind-restore","ephemeral":false}}"#,
            "\n",
            r#"{"event":"session_relationship","data":{"parent_session_id":"old","child_session_id":"aaa","relationship":"rewind-restore","ephemeral":false}}"#,
            "\n",
        );

        let ledger = lineage_ledger_from_jsonl(jsonl, "old").expect("ledger");
        assert_eq!(
            ledger.groups[0].canonical_session_id.as_deref(),
            Some("aaa")
        );
    }

    #[test]
    fn lineage_ledger_empty_source_returns_none() {
        let jsonl = concat!(
            r#"{"event":"session_relationship","data":{"parent_session_id":"parent","child_session_id":"child","relationship":"subagent","ephemeral":false}}"#,
            "\n",
        );
        assert!(lineage_ledger_from_jsonl(jsonl, "  ").is_none());
    }

    #[test]
    fn lineage_ledger_omits_unrelated_relationship_groups() {
        let jsonl = concat!(
            r#"{"event":"session_relationship","data":{"parent_session_id":"parent","child_session_id":"child","relationship":"subagent","ephemeral":false}}"#,
            "\n",
            r#"{"event":"session_relationship","data":{"parent_session_id":"other-parent","child_session_id":"other-child","relationship":"subagent","ephemeral":false}}"#,
            "\n",
        );

        let ledger = lineage_ledger_from_jsonl(jsonl, "child").expect("ledger");
        assert_eq!(ledger.groups.len(), 1);
        assert_eq!(ledger.groups[0].parent_session_id, "parent");
        assert_eq!(ledger.groups[0].branches[0].session_id, "child");
    }

    #[test]
    fn lineage_ledger_parses_fission_branch_relationship() {
        let jsonl = concat!(
            r#"{"event":"session_started","data":{"session_id":"branch","task":"trace the bug"}}"#,
            "\n",
            r#"{"event":"session_relationship","data":{"parent_session_id":"parent","child_session_id":"branch","relationship":"fission-branch","ephemeral":false}}"#,
            "\n",
        );
        let ledger = lineage_ledger_from_jsonl(jsonl, "parent").expect("ledger");
        let branch = &ledger.groups[0].branches[0];
        assert_eq!(branch.relationship, "fission-branch");
        assert_eq!(branch.status, "running");
        assert_eq!(branch.task.as_deref(), Some("trace the bug"));

        // Without markers, a spawn row follows the child's observed lifecycle
        // — and the edge connects the component when sourcing from the child.
        let jsonl = concat!(
            r#"{"event":"session_relationship","data":{"parent_session_id":"parent","child_session_id":"branch","relationship":"fission-branch","ephemeral":false}}"#,
            "\n",
            r#"{"event":"task_complete","data":{"session_id":"branch","summary":"traced"}}"#,
            "\n",
        );
        let ledger = lineage_ledger_from_jsonl(jsonl, "branch").expect("ledger");
        let branch = &ledger.groups[0].branches[0];
        assert_eq!(branch.status, "completed");
        assert_eq!(branch.summary.as_deref(), Some("traced"));
    }

    #[test]
    fn lineage_ledger_fission_detached_updates_spawn_row_without_duplicate() {
        // Detach marker plus a stray later completion: one row, sticky
        // detached — mirrors the fission ledger's rule that a detach must
        // survive completion events from a still-running child.
        let jsonl = concat!(
            r#"{"event":"session_relationship","data":{"parent_session_id":"parent","child_session_id":"branch","relationship":"fission-branch","ephemeral":false}}"#,
            "\n",
            r#"{"event":"session_relationship","data":{"parent_session_id":"parent","child_session_id":"branch","relationship":"fission-detached","ephemeral":false}}"#,
            "\n",
            r#"{"event":"task_complete","data":{"session_id":"branch","summary":"finished anyway"}}"#,
            "\n",
        );
        let ledger = lineage_ledger_from_jsonl(jsonl, "parent").expect("ledger");
        assert_eq!(ledger.groups.len(), 1);
        let branches = &ledger.groups[0].branches;
        assert_eq!(branches.len(), 1);
        assert_eq!(branches[0].relationship, "fission-branch");
        assert_eq!(branches[0].status, "detached");
        // Artifact-level facts (the summary) still render.
        assert_eq!(branches[0].summary.as_deref(), Some("finished anyway"));
    }

    #[test]
    fn lineage_ledger_fission_detached_dedups_regardless_of_event_order() {
        let jsonl = concat!(
            r#"{"event":"session_relationship","data":{"parent_session_id":"parent","child_session_id":"branch","relationship":"fission-detached","ephemeral":false}}"#,
            "\n",
            r#"{"event":"session_relationship","data":{"parent_session_id":"parent","child_session_id":"branch","relationship":"fission-branch","ephemeral":false}}"#,
            "\n",
        );
        let ledger = lineage_ledger_from_jsonl(jsonl, "parent").expect("ledger");
        let branches = &ledger.groups[0].branches;
        assert_eq!(branches.len(), 1);
        assert_eq!(branches[0].relationship, "fission-branch");
        assert_eq!(branches[0].status, "detached");
    }

    #[test]
    fn lineage_ledger_fission_detached_without_spawn_row_gets_own_row() {
        // A truncated log may carry the detach marker but not the spawn
        // event; the fact must stay visible as a standalone row.
        let jsonl = concat!(
            r#"{"event":"session_relationship","data":{"parent_session_id":"parent","child_session_id":"branch","relationship":"fission-detached","ephemeral":false}}"#,
            "\n",
        );
        let ledger = lineage_ledger_from_jsonl(jsonl, "parent").expect("ledger");
        let branches = &ledger.groups[0].branches;
        assert_eq!(branches.len(), 1);
        assert_eq!(branches[0].relationship, "fission-detached");
        assert_eq!(branches[0].status, "detached");
    }

    #[test]
    fn lineage_ledger_fission_imported_marks_spawn_row_status() {
        let jsonl = concat!(
            r#"{"event":"session_relationship","data":{"parent_session_id":"parent","child_session_id":"branch","relationship":"fission-branch","ephemeral":false}}"#,
            "\n",
            r#"{"event":"task_complete","data":{"session_id":"branch","summary":"useful diff"}}"#,
            "\n",
            r#"{"event":"session_relationship","data":{"parent_session_id":"parent","child_session_id":"branch","relationship":"fission-imported","ephemeral":false}}"#,
            "\n",
        );
        let ledger = lineage_ledger_from_jsonl(jsonl, "parent").expect("ledger");
        let branches = &ledger.groups[0].branches;
        assert_eq!(branches.len(), 1);
        assert_eq!(branches[0].relationship, "fission-branch");
        assert_eq!(branches[0].status, "imported");
        assert_eq!(branches[0].summary.as_deref(), Some("useful diff"));
    }

    #[test]
    fn lineage_ledger_import_does_not_resurrect_detached_fission_branch() {
        // Import is artifact-level: a detached branch whose result was
        // salvaged stays detached, mirroring the fission ledger.
        let jsonl = concat!(
            r#"{"event":"session_relationship","data":{"parent_session_id":"parent","child_session_id":"branch","relationship":"fission-branch","ephemeral":false}}"#,
            "\n",
            r#"{"event":"session_relationship","data":{"parent_session_id":"parent","child_session_id":"branch","relationship":"fission-detached","ephemeral":false}}"#,
            "\n",
            r#"{"event":"session_relationship","data":{"parent_session_id":"parent","child_session_id":"branch","relationship":"fission-imported","ephemeral":false}}"#,
            "\n",
        );
        let ledger = lineage_ledger_from_jsonl(jsonl, "parent").expect("ledger");
        let branches = &ledger.groups[0].branches;
        assert_eq!(branches.len(), 1);
        assert_eq!(branches[0].status, "detached");
    }

    #[test]
    fn lineage_ledger_fission_imported_without_spawn_row_gets_own_row() {
        let jsonl = concat!(
            r#"{"event":"session_relationship","data":{"parent_session_id":"parent","child_session_id":"branch","relationship":"fission-imported","ephemeral":false}}"#,
            "\n",
        );
        let ledger = lineage_ledger_from_jsonl(jsonl, "branch").expect("ledger");
        let branches = &ledger.groups[0].branches;
        assert_eq!(branches.len(), 1);
        assert_eq!(branches[0].relationship, "fission-imported");
        assert_eq!(branches[0].status, "imported");
    }

    #[test]
    fn lineage_ledger_fission_marks_do_not_touch_non_fission_rows() {
        // Markers are scoped to fission edges: a subagent edge for the same
        // (parent, child) keeps its own lifecycle status, while the marker —
        // having no spawn row to fold into — renders standalone.
        let jsonl = concat!(
            r#"{"event":"session_relationship","data":{"parent_session_id":"parent","child_session_id":"branch","relationship":"subagent","ephemeral":false}}"#,
            "\n",
            r#"{"event":"session_relationship","data":{"parent_session_id":"parent","child_session_id":"branch","relationship":"fission-detached","ephemeral":false}}"#,
            "\n",
            r#"{"event":"task_complete","data":{"session_id":"branch","summary":"done"}}"#,
            "\n",
        );
        let ledger = lineage_ledger_from_jsonl(jsonl, "parent").expect("ledger");
        let branches = &ledger.groups[0].branches;
        assert_eq!(branches.len(), 2);
        let subagent = branches
            .iter()
            .find(|branch| branch.relationship == "subagent")
            .unwrap();
        let detached = branches
            .iter()
            .find(|branch| branch.relationship == "fission-detached")
            .unwrap();
        assert_eq!(subagent.status, "completed");
        assert_eq!(detached.status, "detached");
    }
}
