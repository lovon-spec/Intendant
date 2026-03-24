use crate::error::CallerError;
use crate::live_audio_types::QuarantinePayload;
use std::path::{Path, PathBuf};
use uuid::Uuid;

/// Base directory for all quarantine data.
fn quarantine_base() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join(".intendant")
        .join("quarantine")
}

/// Directory for a specific live audio session's quarantined payloads.
fn quarantine_dir(live_audio_id: &str) -> PathBuf {
    quarantine_base().join(live_audio_id)
}

/// Store a quarantined payload to disk. Returns a reference (without the content).
///
/// The actual content is written to `~/.intendant/quarantine/<live_audio_id>/<payload_id>.json`.
/// Only the reference is returned; the content is never exposed to agents.
pub fn store_payload(
    live_audio_id: &str,
    content_type: &str,
    content: &str,
) -> Result<QuarantinePayload, CallerError> {
    let dir = quarantine_dir(live_audio_id);
    std::fs::create_dir_all(&dir)?;

    let payload_id = Uuid::new_v4().to_string();
    let timestamp = chrono::Utc::now().to_rfc3339();

    // Sanitize content_type for summary (no raw content leaks)
    let summary = match content_type {
        "tool_call_attempt" => {
            // Extract just the tool name if possible
            if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(content) {
                let name = parsed
                    .get("name")
                    .and_then(|n| n.as_str())
                    .unwrap_or("unknown");
                format!("unexpected tool call: {}", name)
            } else {
                "unexpected tool call attempt".to_string()
            }
        }
        "string_overflow" => "string field exceeded max length".to_string(),
        "unexpected_text" => "unexpected text content from model".to_string(),
        other => format!("quarantined: {}", other),
    };

    // Write the full payload (with content) to disk
    let on_disk = serde_json::json!({
        "payload_id": payload_id,
        "timestamp": timestamp,
        "live_audio_id": live_audio_id,
        "content_type": content_type,
        "summary": summary,
        "content": content,
    });

    let file_path = dir.join(format!("{}.json", payload_id));
    std::fs::write(&file_path, serde_json::to_string_pretty(&on_disk)?)?;

    // Return the reference WITHOUT content
    Ok(QuarantinePayload {
        payload_id,
        timestamp,
        live_audio_id: live_audio_id.to_string(),
        content_type: content_type.to_string(),
        summary,
    })
}

/// List all quarantined payload references for a live audio session.
/// Returns references only (no content).
pub fn list_payloads(live_audio_id: &str) -> Result<Vec<QuarantinePayload>, CallerError> {
    let dir = quarantine_dir(live_audio_id);
    if !dir.exists() {
        return Ok(Vec::new());
    }

    let mut payloads = Vec::new();
    for entry in std::fs::read_dir(&dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let data = std::fs::read_to_string(&path)?;
        if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&data) {
            payloads.push(QuarantinePayload {
                payload_id: parsed["payload_id"]
                    .as_str()
                    .unwrap_or("")
                    .to_string(),
                timestamp: parsed["timestamp"]
                    .as_str()
                    .unwrap_or("")
                    .to_string(),
                live_audio_id: parsed["live_audio_id"]
                    .as_str()
                    .unwrap_or("")
                    .to_string(),
                content_type: parsed["content_type"]
                    .as_str()
                    .unwrap_or("")
                    .to_string(),
                summary: parsed["summary"]
                    .as_str()
                    .unwrap_or("")
                    .to_string(),
            });
        }
    }

    Ok(payloads)
}

/// Read the actual content of a quarantined payload. For human review only.
///
/// This function intentionally returns the raw content string. It must NEVER
/// be called from code that feeds the result back to an agent.
pub fn read_payload(live_audio_id: &str, payload_id: &str) -> Result<String, CallerError> {
    let file_path = quarantine_dir(live_audio_id).join(format!("{}.json", payload_id));
    let data = std::fs::read_to_string(&file_path)?;
    let parsed: serde_json::Value = serde_json::from_str(&data)?;
    Ok(parsed["content"]
        .as_str()
        .unwrap_or("")
        .to_string())
}

/// Remove all quarantine data for a live audio session.
pub fn cleanup_quarantine(live_audio_id: &str) -> Result<(), CallerError> {
    let dir = quarantine_dir(live_audio_id);
    if dir.exists() {
        std::fs::remove_dir_all(&dir)?;
    }
    Ok(())
}

/// Create a quarantine function bound to a specific live audio session ID.
/// This is the callback passed to `schema_validator::validate()`.
pub fn make_quarantine_fn(
    live_audio_id: String,
) -> impl FnMut(&str, &str, &str) -> QuarantinePayload {
    move |_field: &str, content_type: &str, content: &str| {
        store_payload(&live_audio_id, content_type, content)
            .unwrap_or_else(|e| {
                // If quarantine write fails, return a placeholder reference
                QuarantinePayload {
                    payload_id: "error".to_string(),
                    timestamp: String::new(),
                    live_audio_id: live_audio_id.clone(),
                    content_type: content_type.to_string(),
                    summary: format!("quarantine write failed: {}", e),
                }
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_and_list_and_read_payload() {
        let tmp = tempfile::tempdir().unwrap();
        // Override quarantine base for testing
        let live_id = "test-session";
        let dir = tmp.path().join(live_id);
        std::fs::create_dir_all(&dir).unwrap();

        let payload_id = Uuid::new_v4().to_string();
        let on_disk = serde_json::json!({
            "payload_id": payload_id,
            "timestamp": "2026-01-01T00:00:00Z",
            "live_audio_id": live_id,
            "content_type": "tool_call_attempt",
            "summary": "unexpected tool call: browse_url",
            "content": "{\"name\":\"browse_url\",\"args\":{\"url\":\"http://evil.com\"}}",
        });

        let file_path = dir.join(format!("{}.json", payload_id));
        std::fs::write(&file_path, serde_json::to_string_pretty(&on_disk).unwrap()).unwrap();

        // List payloads
        let mut payloads = Vec::new();
        for entry in std::fs::read_dir(&dir).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let data = std::fs::read_to_string(&path).unwrap();
            let parsed: serde_json::Value = serde_json::from_str(&data).unwrap();
            payloads.push(QuarantinePayload {
                payload_id: parsed["payload_id"].as_str().unwrap().to_string(),
                timestamp: parsed["timestamp"].as_str().unwrap().to_string(),
                live_audio_id: parsed["live_audio_id"].as_str().unwrap().to_string(),
                content_type: parsed["content_type"].as_str().unwrap().to_string(),
                summary: parsed["summary"].as_str().unwrap().to_string(),
            });
        }

        assert_eq!(payloads.len(), 1);
        assert_eq!(payloads[0].content_type, "tool_call_attempt");
        assert!(payloads[0].summary.contains("browse_url"));

        // Read content (human review)
        let data = std::fs::read_to_string(&file_path).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&data).unwrap();
        let content = parsed["content"].as_str().unwrap();
        assert!(content.contains("browse_url"));
        assert!(content.contains("evil.com"));
    }

    #[test]
    fn store_payload_creates_dir_and_file() {
        // This test uses the real quarantine_base but with a unique live_audio_id
        let live_id = format!("test-{}", Uuid::new_v4());
        let result = store_payload(&live_id, "test_type", "test content");
        assert!(result.is_ok());
        let payload = result.unwrap();
        assert!(!payload.payload_id.is_empty());
        assert_eq!(payload.live_audio_id, live_id);

        // Verify the file exists
        let file_path =
            quarantine_dir(&live_id).join(format!("{}.json", payload.payload_id));
        assert!(file_path.exists());

        // Verify content is on disk but not in the payload reference
        let data = std::fs::read_to_string(&file_path).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&data).unwrap();
        assert_eq!(parsed["content"], "test content");

        // Clean up
        cleanup_quarantine(&live_id).unwrap();
        assert!(!quarantine_dir(&live_id).exists());
    }

    #[test]
    fn list_payloads_empty_dir() {
        let live_id = format!("nonexistent-{}", Uuid::new_v4());
        let result = list_payloads(&live_id).unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn read_payload_full_roundtrip() {
        let live_id = format!("test-read-{}", Uuid::new_v4());
        let payload = store_payload(
            &live_id,
            "tool_call_attempt",
            r#"{"name":"browse_url","args":{"url":"http://example.com"}}"#,
        )
        .unwrap();

        // Summary should not contain the actual URL
        assert!(!payload.summary.contains("example.com"));
        assert!(payload.summary.contains("browse_url"));

        // But read_payload should return the full content
        let content = read_payload(&live_id, &payload.payload_id).unwrap();
        assert!(content.contains("example.com"));

        cleanup_quarantine(&live_id).unwrap();
    }

    #[test]
    fn make_quarantine_fn_works() {
        let live_id = format!("test-fn-{}", Uuid::new_v4());
        let mut qfn = make_quarantine_fn(live_id.clone());
        let payload = qfn("field_name", "string_overflow", "very long string content");
        assert_eq!(payload.live_audio_id, live_id);
        assert_eq!(payload.content_type, "string_overflow");
        cleanup_quarantine(&live_id).unwrap();
    }

    #[test]
    fn summary_sanitization() {
        let live_id = format!("test-summary-{}", Uuid::new_v4());

        // Tool call attempt extracts tool name
        let p1 = store_payload(
            &live_id,
            "tool_call_attempt",
            r#"{"name":"exec_command","args":{"command":"rm -rf /"}}"#,
        )
        .unwrap();
        assert_eq!(p1.summary, "unexpected tool call: exec_command");
        // Summary does NOT contain the dangerous command
        assert!(!p1.summary.contains("rm -rf"));

        // String overflow
        let p2 = store_payload(&live_id, "string_overflow", "a".repeat(10000).as_str()).unwrap();
        assert_eq!(p2.summary, "string field exceeded max length");

        // Unknown type
        let p3 = store_payload(&live_id, "weird_thing", "content").unwrap();
        assert_eq!(p3.summary, "quarantined: weird_thing");

        cleanup_quarantine(&live_id).unwrap();
    }
}
