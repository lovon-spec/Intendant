#![allow(dead_code)]

use crate::error::CallerError;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KnowledgeEntry {
    pub id: String,
    pub key: String,
    pub summary: String,
    pub tags: Vec<String>,
    pub source: String,
    pub channel: String,
    pub created_at: u64,
    pub updated_at: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KnowledgeStore {
    pub entries: Vec<KnowledgeEntry>,
    #[serde(default)]
    pub subscriptions: HashMap<String, Vec<String>>,
    #[serde(default)]
    pub cursors: HashMap<String, usize>,
}

impl KnowledgeStore {
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
            subscriptions: HashMap::new(),
            cursors: HashMap::new(),
        }
    }
}

impl Default for KnowledgeStore {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, Default)]
pub struct KnowledgeQuery {
    pub tags: Option<Vec<String>>,
    pub channel: Option<String>,
    pub source: Option<String>,
    pub keywords: Option<Vec<String>>,
    pub since: Option<u64>,
}

pub fn publish(store: &mut KnowledgeStore, entry: KnowledgeEntry) {
    // Check if entry with same key and source exists
    if let Some(existing) = store
        .entries
        .iter_mut()
        .find(|e| e.key == entry.key && e.source == entry.source)
    {
        existing.summary = entry.summary;
        existing.tags = entry.tags;
        existing.channel = entry.channel;
        existing.updated_at = entry.updated_at;
    } else {
        store.entries.push(entry);
    }
}

pub fn query<'a>(store: &'a KnowledgeStore, q: &KnowledgeQuery) -> Vec<&'a KnowledgeEntry> {
    store
        .entries
        .iter()
        .filter(|e| {
            if let Some(ref tags) = q.tags {
                if !tags.iter().any(|t| e.tags.contains(t)) {
                    return false;
                }
            }
            if let Some(ref channel) = q.channel {
                if &e.channel != channel {
                    return false;
                }
            }
            if let Some(ref source) = q.source {
                if &e.source != source {
                    return false;
                }
            }
            if let Some(since) = q.since {
                if e.updated_at < since {
                    return false;
                }
            }
            if let Some(ref keywords) = q.keywords {
                let key_lower = e.key.to_lowercase();
                let summary_lower = e.summary.to_lowercase();
                if !keywords.iter().any(|kw| {
                    let kw_lower = kw.to_lowercase();
                    key_lower.contains(&kw_lower) || summary_lower.contains(&kw_lower)
                }) {
                    return false;
                }
            }
            true
        })
        .collect()
}

pub fn subscribe(store: &mut KnowledgeStore, agent_id: &str, channel: &str) {
    store
        .subscriptions
        .entry(agent_id.to_string())
        .or_default()
        .push(channel.to_string());

    // Deduplicate
    if let Some(channels) = store.subscriptions.get_mut(agent_id) {
        channels.sort();
        channels.dedup();
    }
}

pub fn get_unseen<'a>(store: &'a KnowledgeStore, agent_id: &str) -> Vec<&'a KnowledgeEntry> {
    let cursor = store.cursors.get(agent_id).copied().unwrap_or(0);
    let channels = match store.subscriptions.get(agent_id) {
        Some(ch) => ch,
        None => return vec![],
    };

    store.entries[cursor..]
        .iter()
        .filter(|e| channels.contains(&e.channel))
        .collect()
}

pub fn advance_cursor(store: &mut KnowledgeStore, agent_id: &str) {
    store
        .cursors
        .insert(agent_id.to_string(), store.entries.len());
}

pub fn load(path: &Path) -> Result<KnowledgeStore, CallerError> {
    if !path.exists() {
        return Ok(KnowledgeStore::new());
    }

    let content = std::fs::read_to_string(path)?;

    // Try new format first (Vec entries)
    if let Ok(store) = serde_json::from_str::<KnowledgeStore>(&content) {
        return Ok(store);
    }

    // Try old format (HashMap entries) and migrate
    migrate_from_old_format(&content)
}

fn migrate_from_old_format(content: &str) -> Result<KnowledgeStore, CallerError> {
    #[derive(Deserialize)]
    struct OldMemoryEntry {
        summary: String,
        created_at: u64,
        updated_at: u64,
    }

    #[derive(Deserialize)]
    struct OldMemoryStore {
        entries: HashMap<String, OldMemoryEntry>,
    }

    let old_store: OldMemoryStore = serde_json::from_str(content).map_err(CallerError::Json)?;

    let mut new_store = KnowledgeStore::new();
    let mut id_counter = 0u64;

    for (key, old_entry) in old_store.entries {
        id_counter += 1;
        new_store.entries.push(KnowledgeEntry {
            id: id_counter.to_string(),
            key,
            summary: old_entry.summary,
            tags: vec![],
            source: "migrated".to_string(),
            channel: "default".to_string(),
            created_at: old_entry.created_at,
            updated_at: old_entry.updated_at,
        });
    }

    Ok(new_store)
}

pub fn save(store: &KnowledgeStore, path: &Path) -> Result<(), CallerError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let content = serde_json::to_string_pretty(store).map_err(|e| {
        CallerError::SubAgent(format!("Failed to serialize knowledge store: {}", e))
    })?;
    std::fs::write(path, content)?;
    Ok(())
}

pub fn format_for_injection(entries: &[&KnowledgeEntry]) -> String {
    if entries.is_empty() {
        return String::new();
    }

    let mut msg = String::from("[Knowledge Update]\n\n");
    for entry in entries {
        msg.push_str(&format!(
            "- **{}** ({}): {}",
            entry.key, entry.channel, entry.summary
        ));
        if !entry.tags.is_empty() {
            msg.push_str(&format!(" [tags: {}]", entry.tags.join(", ")));
        }
        msg.push('\n');
    }
    msg
}

pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

pub fn route_knowledge<'a>(
    store: &'a KnowledgeStore,
    active_agent_ids: &[String],
) -> HashMap<String, Vec<&'a KnowledgeEntry>> {
    let mut routes: HashMap<String, Vec<&'a KnowledgeEntry>> = HashMap::new();

    for agent_id in active_agent_ids {
        let unseen = get_unseen(store, agent_id);
        if !unseen.is_empty() {
            routes.insert(agent_id.clone(), unseen);
        }
    }

    routes
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_entry(key: &str, summary: &str, channel: &str, tags: &[&str]) -> KnowledgeEntry {
        KnowledgeEntry {
            id: key.to_string(),
            key: key.to_string(),
            summary: summary.to_string(),
            tags: tags.iter().map(|t| t.to_string()).collect(),
            source: "test-agent".to_string(),
            channel: channel.to_string(),
            created_at: 1000,
            updated_at: 1000,
        }
    }

    #[test]
    fn new_store_is_empty() {
        let store = KnowledgeStore::new();
        assert!(store.entries.is_empty());
        assert!(store.subscriptions.is_empty());
        assert!(store.cursors.is_empty());
    }

    #[test]
    fn publish_new_entry() {
        let mut store = KnowledgeStore::new();
        let entry = make_entry(
            "db-schema",
            "PostgreSQL with 5 tables",
            "findings",
            &["database"],
        );
        publish(&mut store, entry);
        assert_eq!(store.entries.len(), 1);
        assert_eq!(store.entries[0].key, "db-schema");
    }

    #[test]
    fn publish_updates_existing() {
        let mut store = KnowledgeStore::new();
        let entry1 = make_entry(
            "db-schema",
            "PostgreSQL with 5 tables",
            "findings",
            &["database"],
        );
        publish(&mut store, entry1);

        let mut entry2 = make_entry(
            "db-schema",
            "PostgreSQL with 7 tables",
            "findings",
            &["database"],
        );
        entry2.updated_at = 2000;
        publish(&mut store, entry2);

        assert_eq!(store.entries.len(), 1);
        assert_eq!(store.entries[0].summary, "PostgreSQL with 7 tables");
        assert_eq!(store.entries[0].updated_at, 2000);
    }

    #[test]
    fn query_by_tags() {
        let mut store = KnowledgeStore::new();
        publish(
            &mut store,
            make_entry("e1", "db stuff", "findings", &["database"]),
        );
        publish(
            &mut store,
            make_entry("e2", "api stuff", "findings", &["api"]),
        );
        publish(
            &mut store,
            make_entry("e3", "db api", "findings", &["database", "api"]),
        );

        let results = query(
            &store,
            &KnowledgeQuery {
                tags: Some(vec!["database".to_string()]),
                ..Default::default()
            },
        );
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn query_by_channel() {
        let mut store = KnowledgeStore::new();
        publish(&mut store, make_entry("e1", "finding", "findings", &[]));
        publish(&mut store, make_entry("e2", "decision", "decisions", &[]));

        let results = query(
            &store,
            &KnowledgeQuery {
                channel: Some("findings".to_string()),
                ..Default::default()
            },
        );
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].key, "e1");
    }

    #[test]
    fn query_by_source() {
        let mut store = KnowledgeStore::new();
        let mut e1 = make_entry("e1", "from agent1", "findings", &[]);
        e1.source = "agent-1".to_string();
        publish(&mut store, e1);

        let mut e2 = make_entry("e2", "from agent2", "findings", &[]);
        e2.source = "agent-2".to_string();
        publish(&mut store, e2);

        let results = query(
            &store,
            &KnowledgeQuery {
                source: Some("agent-1".to_string()),
                ..Default::default()
            },
        );
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].key, "e1");
    }

    #[test]
    fn query_by_keywords() {
        let mut store = KnowledgeStore::new();
        publish(
            &mut store,
            make_entry("db-schema", "PostgreSQL tables", "findings", &[]),
        );
        publish(
            &mut store,
            make_entry("api-routes", "REST endpoints", "findings", &[]),
        );

        let results = query(
            &store,
            &KnowledgeQuery {
                keywords: Some(vec!["postgresql".to_string()]),
                ..Default::default()
            },
        );
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].key, "db-schema");
    }

    #[test]
    fn query_by_since() {
        let mut store = KnowledgeStore::new();
        let mut e1 = make_entry("old", "old entry", "findings", &[]);
        e1.updated_at = 100;
        publish(&mut store, e1);

        let mut e2 = make_entry("new", "new entry", "findings", &[]);
        e2.updated_at = 200;
        publish(&mut store, e2);

        let results = query(
            &store,
            &KnowledgeQuery {
                since: Some(150),
                ..Default::default()
            },
        );
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].key, "new");
    }

    #[test]
    fn query_combined_filters() {
        let mut store = KnowledgeStore::new();
        publish(
            &mut store,
            make_entry("e1", "db tables", "findings", &["database"]),
        );
        publish(
            &mut store,
            make_entry("e2", "api routes", "findings", &["api"]),
        );
        publish(
            &mut store,
            make_entry("e3", "db backup", "decisions", &["database"]),
        );

        let results = query(
            &store,
            &KnowledgeQuery {
                tags: Some(vec!["database".to_string()]),
                channel: Some("findings".to_string()),
                ..Default::default()
            },
        );
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].key, "e1");
    }

    #[test]
    fn query_no_filters_returns_all() {
        let mut store = KnowledgeStore::new();
        publish(&mut store, make_entry("e1", "one", "findings", &[]));
        publish(&mut store, make_entry("e2", "two", "findings", &[]));

        let results = query(&store, &KnowledgeQuery::default());
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn subscribe_and_get_unseen() {
        let mut store = KnowledgeStore::new();

        // Add entries before subscribing
        publish(&mut store, make_entry("e1", "old", "findings", &[]));

        // Subscribe agent
        subscribe(&mut store, "agent-1", "findings");

        // Agent hasn't seen anything yet (cursor at 0)
        let unseen = get_unseen(&store, "agent-1");
        assert_eq!(unseen.len(), 1);

        // Advance cursor
        advance_cursor(&mut store, "agent-1");
        let unseen = get_unseen(&store, "agent-1");
        assert_eq!(unseen.len(), 0);

        // Add new entry
        publish(&mut store, make_entry("e2", "new", "findings", &[]));
        let unseen = get_unseen(&store, "agent-1");
        assert_eq!(unseen.len(), 1);
        assert_eq!(unseen[0].key, "e2");
    }

    #[test]
    fn subscribe_filters_by_channel() {
        let mut store = KnowledgeStore::new();
        subscribe(&mut store, "agent-1", "findings");

        publish(&mut store, make_entry("e1", "finding", "findings", &[]));
        publish(&mut store, make_entry("e2", "decision", "decisions", &[]));

        let unseen = get_unseen(&store, "agent-1");
        assert_eq!(unseen.len(), 1);
        assert_eq!(unseen[0].key, "e1");
    }

    #[test]
    fn subscribe_deduplicates() {
        let mut store = KnowledgeStore::new();
        subscribe(&mut store, "agent-1", "findings");
        subscribe(&mut store, "agent-1", "findings");
        subscribe(&mut store, "agent-1", "findings");

        assert_eq!(store.subscriptions["agent-1"].len(), 1);
    }

    #[test]
    fn get_unseen_unsubscribed_agent() {
        let store = KnowledgeStore::new();
        let unseen = get_unseen(&store, "unknown-agent");
        assert!(unseen.is_empty());
    }

    #[test]
    fn save_and_load_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("knowledge.json");

        let mut store = KnowledgeStore::new();
        publish(
            &mut store,
            make_entry("db", "PostgreSQL", "findings", &["database"]),
        );
        subscribe(&mut store, "agent-1", "findings");
        advance_cursor(&mut store, "agent-1");

        save(&store, &path).unwrap();
        let loaded = load(&path).unwrap();

        assert_eq!(loaded.entries.len(), 1);
        assert_eq!(loaded.entries[0].key, "db");
        assert_eq!(
            loaded.subscriptions["agent-1"],
            vec!["findings".to_string()]
        );
        assert_eq!(loaded.cursors["agent-1"], 1);
    }

    #[test]
    fn load_nonexistent_returns_empty() {
        let store = load(Path::new("/nonexistent/knowledge.json")).unwrap();
        assert!(store.entries.is_empty());
    }

    #[test]
    fn load_old_format_migrates() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("memory.json");

        let old_content = r#"{
            "entries": {
                "db-config": {"summary": "PostgreSQL on port 5432", "created_at": 1000, "updated_at": 2000},
                "api-key": {"summary": "Uses JWT tokens", "created_at": 1100, "updated_at": 2100}
            }
        }"#;
        std::fs::write(&path, old_content).unwrap();

        let store = load(&path).unwrap();
        assert_eq!(store.entries.len(), 2);

        let keys: Vec<&str> = store.entries.iter().map(|e| e.key.as_str()).collect();
        assert!(keys.contains(&"db-config"));
        assert!(keys.contains(&"api-key"));

        // Verify migration metadata
        for entry in &store.entries {
            assert_eq!(entry.source, "migrated");
            assert_eq!(entry.channel, "default");
        }
    }

    #[test]
    fn load_invalid_json_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("knowledge.json");
        std::fs::write(&path, "not valid json").unwrap();
        assert!(load(&path).is_err());
    }

    #[test]
    fn format_for_injection_empty() {
        let msg = format_for_injection(&[]);
        assert!(msg.is_empty());
    }

    #[test]
    fn format_for_injection_with_entries() {
        let e1 = make_entry("db", "PostgreSQL tables", "findings", &["database"]);
        let e2 = make_entry("api", "REST endpoints", "findings", &[]);
        let entries: Vec<&KnowledgeEntry> = vec![&e1, &e2];

        let msg = format_for_injection(&entries);
        assert!(msg.contains("[Knowledge Update]"));
        assert!(msg.contains("**db**"));
        assert!(msg.contains("PostgreSQL tables"));
        assert!(msg.contains("[tags: database]"));
        assert!(msg.contains("**api**"));
        assert!(!msg.contains("[tags:") || msg.matches("[tags:").count() == 1);
    }

    #[test]
    fn route_knowledge_basic() {
        let mut store = KnowledgeStore::new();
        subscribe(&mut store, "agent-1", "findings");
        subscribe(&mut store, "agent-2", "decisions");

        publish(&mut store, make_entry("e1", "finding", "findings", &[]));
        publish(&mut store, make_entry("e2", "decision", "decisions", &[]));

        let agent_ids = vec!["agent-1".to_string(), "agent-2".to_string()];
        let routes = route_knowledge(&store, &agent_ids);

        assert_eq!(routes["agent-1"].len(), 1);
        assert_eq!(routes["agent-1"][0].key, "e1");
        assert_eq!(routes["agent-2"].len(), 1);
        assert_eq!(routes["agent-2"][0].key, "e2");
    }

    #[test]
    fn route_knowledge_no_unseen() {
        let mut store = KnowledgeStore::new();
        subscribe(&mut store, "agent-1", "findings");
        publish(&mut store, make_entry("e1", "finding", "findings", &[]));
        advance_cursor(&mut store, "agent-1");

        let agent_ids = vec!["agent-1".to_string()];
        let routes = route_knowledge(&store, &agent_ids);
        assert!(routes.is_empty());
    }

    #[test]
    fn full_pub_sub_lifecycle() {
        let mut store = KnowledgeStore::new();

        // Two agents subscribe to different channels
        subscribe(&mut store, "research-1", "findings");
        subscribe(&mut store, "impl-1", "findings");
        subscribe(&mut store, "impl-1", "decisions");

        // Research publishes findings
        let mut finding = make_entry("schema", "5 tables found", "findings", &["database"]);
        finding.source = "research-1".to_string();
        publish(&mut store, finding);

        // Both agents should see the finding
        assert_eq!(get_unseen(&store, "research-1").len(), 1);
        assert_eq!(get_unseen(&store, "impl-1").len(), 1);

        // Research advances its cursor
        advance_cursor(&mut store, "research-1");
        assert_eq!(get_unseen(&store, "research-1").len(), 0);

        // Orchestrator publishes a decision
        let mut decision = make_entry("db-choice", "Use PostgreSQL", "decisions", &["database"]);
        decision.source = "orchestrator".to_string();
        publish(&mut store, decision);

        // research-1 doesn't subscribe to decisions
        assert_eq!(get_unseen(&store, "research-1").len(), 0);
        // impl-1 subscribes to both
        assert_eq!(get_unseen(&store, "impl-1").len(), 2);

        // impl-1 advances cursor
        advance_cursor(&mut store, "impl-1");
        assert_eq!(get_unseen(&store, "impl-1").len(), 0);
    }
}
