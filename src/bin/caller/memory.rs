use crate::knowledge::{self, KnowledgeStore};
use crate::project::Project;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;

// Backward-compatible types (kept for reference and migration)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryEntry {
    pub summary: String,
    pub created_at: u64,
    pub updated_at: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryStore {
    pub entries: HashMap<String, MemoryEntry>,
}

pub fn load_memory(project: &Project) -> Option<MemoryStore> {
    if !project.config.memory.enabled {
        return None;
    }

    let path = project.memory_path();
    if !path.exists() {
        return None;
    }

    let content = fs::read_to_string(&path).ok()?;

    // Try loading as old HashMap format first (for backward compat)
    if let Ok(store) = serde_json::from_str::<MemoryStore>(&content) {
        return Some(store);
    }

    // Try loading as new KnowledgeStore format and convert back
    if let Ok(kstore) = serde_json::from_str::<KnowledgeStore>(&content) {
        let mut entries = HashMap::new();
        for entry in &kstore.entries {
            entries.insert(
                entry.key.clone(),
                MemoryEntry {
                    summary: entry.summary.clone(),
                    created_at: entry.created_at,
                    updated_at: entry.updated_at,
                },
            );
        }
        return Some(MemoryStore { entries });
    }

    None
}

#[allow(dead_code)]
pub fn load_knowledge(project: &Project) -> Option<KnowledgeStore> {
    if !project.config.memory.enabled {
        return None;
    }

    let path = project.memory_path();
    knowledge::load(&path).ok()
}

pub fn format_memory_message(store: &MemoryStore) -> Option<String> {
    if store.entries.is_empty() {
        return None;
    }

    let mut entries: Vec<(&String, &MemoryEntry)> = store.entries.iter().collect();
    entries.sort_by(|a, b| b.1.updated_at.cmp(&a.1.updated_at));

    let mut msg = String::from("[Project Memory]\n\n");
    for (key, entry) in entries {
        msg.push_str(&format!("- **{}**: {}\n", key, entry.summary));
    }
    Some(msg)
}

#[allow(dead_code)]
pub fn format_knowledge_message(store: &KnowledgeStore) -> Option<String> {
    if store.entries.is_empty() {
        return None;
    }

    let refs: Vec<&knowledge::KnowledgeEntry> = store.entries.iter().collect();
    let msg = knowledge::format_for_injection(&refs);
    if msg.is_empty() {
        None
    } else {
        Some(msg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::{Project, ProjectConfig};
    use std::path::PathBuf;

    fn make_project(root: PathBuf) -> Project {
        Project {
            root,
            config: ProjectConfig::default(),
        }
    }

    #[test]
    fn load_memory_nonexistent() {
        let dir = tempfile::tempdir().unwrap();
        let project = make_project(dir.path().to_path_buf());
        // memory.enabled defaults to true, but file doesn't exist
        assert!(load_memory(&project).is_none());
    }

    #[test]
    fn load_memory_disabled() {
        let dir = tempfile::tempdir().unwrap();
        let mut project = make_project(dir.path().to_path_buf());
        project.config.memory.enabled = false;
        assert!(load_memory(&project).is_none());
    }

    #[test]
    fn load_memory_valid() {
        let dir = tempfile::tempdir().unwrap();
        let project = make_project(dir.path().to_path_buf());
        let mem_path = project.memory_path();
        fs::create_dir_all(mem_path.parent().unwrap()).unwrap();
        fs::write(
            &mem_path,
            r#"{"entries":{"db":{"summary":"PostgreSQL","created_at":1000,"updated_at":2000}}}"#,
        )
        .unwrap();

        let store = load_memory(&project).unwrap();
        assert_eq!(store.entries.len(), 1);
        assert_eq!(store.entries["db"].summary, "PostgreSQL");
    }

    #[test]
    fn load_memory_invalid_json() {
        let dir = tempfile::tempdir().unwrap();
        let project = make_project(dir.path().to_path_buf());
        let mem_path = project.memory_path();
        fs::create_dir_all(mem_path.parent().unwrap()).unwrap();
        fs::write(&mem_path, "not json").unwrap();

        assert!(load_memory(&project).is_none());
    }

    #[test]
    fn format_memory_message_empty() {
        let store = MemoryStore {
            entries: HashMap::new(),
        };
        assert!(format_memory_message(&store).is_none());
    }

    #[test]
    fn format_memory_message_sorted_by_recency() {
        let mut entries = HashMap::new();
        entries.insert(
            "old".to_string(),
            MemoryEntry {
                summary: "Old entry".to_string(),
                created_at: 100,
                updated_at: 100,
            },
        );
        entries.insert(
            "new".to_string(),
            MemoryEntry {
                summary: "New entry".to_string(),
                created_at: 200,
                updated_at: 200,
            },
        );

        let store = MemoryStore { entries };
        let msg = format_memory_message(&store).unwrap();
        assert!(msg.contains("[Project Memory]"));
        // "new" should appear before "old"
        let new_pos = msg.find("**new**").unwrap();
        let old_pos = msg.find("**old**").unwrap();
        assert!(new_pos < old_pos);
    }

    #[test]
    fn load_memory_from_knowledge_format() {
        let dir = tempfile::tempdir().unwrap();
        let project = make_project(dir.path().to_path_buf());
        let mem_path = project.memory_path();
        fs::create_dir_all(mem_path.parent().unwrap()).unwrap();

        let knowledge_content = r#"{
            "entries": [
                {"id": "1", "key": "db", "summary": "PostgreSQL", "tags": ["database"], "source": "agent", "channel": "findings", "created_at": 1000, "updated_at": 2000}
            ],
            "subscriptions": {},
            "cursors": {}
        }"#;
        fs::write(&mem_path, knowledge_content).unwrap();

        let store = load_memory(&project).unwrap();
        assert_eq!(store.entries.len(), 1);
        assert_eq!(store.entries["db"].summary, "PostgreSQL");
    }

    #[test]
    fn load_knowledge_from_old_format() {
        let dir = tempfile::tempdir().unwrap();
        let project = make_project(dir.path().to_path_buf());
        let mem_path = project.memory_path();
        fs::create_dir_all(mem_path.parent().unwrap()).unwrap();

        let old_content =
            r#"{"entries":{"db":{"summary":"PostgreSQL","created_at":1000,"updated_at":2000}}}"#;
        fs::write(&mem_path, old_content).unwrap();

        let kstore = load_knowledge(&project).unwrap();
        assert_eq!(kstore.entries.len(), 1);
        assert_eq!(kstore.entries[0].key, "db");
        assert_eq!(kstore.entries[0].source, "migrated");
    }

    #[test]
    fn load_knowledge_disabled() {
        let dir = tempfile::tempdir().unwrap();
        let mut project = make_project(dir.path().to_path_buf());
        project.config.memory.enabled = false;
        assert!(load_knowledge(&project).is_none());
    }

    #[test]
    fn format_knowledge_message_empty() {
        let store = KnowledgeStore::new();
        assert!(format_knowledge_message(&store).is_none());
    }

    #[test]
    fn format_knowledge_message_with_entries() {
        let mut store = KnowledgeStore::new();
        store.entries.push(knowledge::KnowledgeEntry {
            id: "1".to_string(),
            key: "db".to_string(),
            summary: "PostgreSQL".to_string(),
            tags: vec!["database".to_string()],
            source: "agent".to_string(),
            channel: "findings".to_string(),
            created_at: 1000,
            updated_at: 2000,
        });

        let msg = format_knowledge_message(&store).unwrap();
        assert!(msg.contains("[Knowledge Update]"));
        assert!(msg.contains("**db**"));
        assert!(msg.contains("PostgreSQL"));
    }
}
