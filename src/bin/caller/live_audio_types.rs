use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Provider for the live audio model connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LiveAudioProvider {
    Gemini,
    OpenAI,
}

/// What the parent agent provides to spawn a live audio session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LiveAudioSpec {
    pub id: String,
    pub provider: LiveAudioProvider,
    #[serde(default)]
    pub model: Option<String>,
    pub playbook: String,
    #[serde(default)]
    pub response_schema: ResponseSchema,
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
    #[serde(default)]
    pub voice: Option<String>,
    #[serde(default)]
    pub display_id: Option<u32>,
    /// Optional text sent to the model after setup completes, before audio
    /// bridging begins. Use this to prompt the model when the other party is
    /// already on the line (e.g. "The call has connected, introduce yourself.").
    /// When None, the model waits for audio input from the other party.
    #[serde(default)]
    pub initial_message: Option<String>,
}

fn default_timeout() -> u64 {
    300
}

/// Constrained response schema that the live model must produce.
///
/// The parent defines this schema and the live model's output is validated
/// against it programmatically. String fields support constraints (max length,
/// enum values) and a "tainted" marker for fields that may contain injected text.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ResponseSchema {
    pub fields: Vec<FieldSpec>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FieldSpec {
    pub name: String,
    pub field_type: FieldType,
    #[serde(default)]
    pub required: bool,
    #[serde(default)]
    pub description: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum FieldType {
    String {
        #[serde(default)]
        max_length: Option<usize>,
        #[serde(default)]
        allowed_values: Option<Vec<String>>,
        /// When true, the parent must treat this field's content as opaque data
        /// that may contain injected text. It will not be interpreted as instructions.
        #[serde(default)]
        tainted: bool,
    },
    Integer {
        #[serde(default)]
        min: Option<i64>,
        #[serde(default)]
        max: Option<i64>,
    },
    Boolean,
    Array {
        element_type: Box<FieldType>,
        #[serde(default)]
        max_items: Option<usize>,
    },
}

/// Result returned from a completed live audio session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LiveAudioResult {
    pub id: String,
    pub status: LiveAudioStatus,
    pub response_data: Option<serde_json::Value>,
    pub quarantine_ids: Vec<String>,
    pub transcript_path: PathBuf,
    pub duration_secs: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum LiveAudioStatus {
    Completed,
    TimedOut,
    Disconnected,
    SchemaError(String),
    Failed(String),
}

/// Progress update from a running live audio session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LiveAudioProgress {
    pub id: String,
    pub state: LiveAudioState,
    pub elapsed_secs: f64,
    pub transcript_preview: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum LiveAudioState {
    Connecting,
    SetupComplete,
    Speaking,
    Listening,
    Finished,
}

/// A quarantined payload reference. The actual content is stored on disk only;
/// this struct intentionally does NOT contain the content itself.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuarantinePayload {
    pub payload_id: String,
    pub timestamp: String,
    pub live_audio_id: String,
    pub content_type: String,
    pub summary: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn live_audio_provider_serde_roundtrip() {
        let gemini = LiveAudioProvider::Gemini;
        let json = serde_json::to_string(&gemini).unwrap();
        assert_eq!(json, "\"gemini\"");
        let back: LiveAudioProvider = serde_json::from_str(&json).unwrap();
        assert_eq!(back, gemini);

        let openai = LiveAudioProvider::OpenAI;
        let json = serde_json::to_string(&openai).unwrap();
        assert_eq!(json, "\"openai\"");
        let back: LiveAudioProvider = serde_json::from_str(&json).unwrap();
        assert_eq!(back, openai);
    }

    #[test]
    fn live_audio_spec_serde_roundtrip() {
        let spec = LiveAudioSpec {
            id: "call-1".to_string(),
            provider: LiveAudioProvider::Gemini,
            model: Some("gemini-2.5-flash".to_string()),
            playbook: "You are calling a pizza restaurant.".to_string(),
            response_schema: ResponseSchema {
                fields: vec![
                    FieldSpec {
                        name: "order_confirmed".to_string(),
                        field_type: FieldType::Boolean,
                        required: true,
                        description: Some("Whether the order was confirmed".to_string()),
                    },
                    FieldSpec {
                        name: "confirmation_number".to_string(),
                        field_type: FieldType::String {
                            max_length: Some(20),
                            allowed_values: None,
                            tainted: false,
                        },
                        required: false,
                        description: None,
                    },
                ],
            },
            timeout_secs: 120,
            voice: Some("Aoede".to_string()),
            display_id: Some(99),
            initial_message: None,
        };

        let json = serde_json::to_string_pretty(&spec).unwrap();
        let back: LiveAudioSpec = serde_json::from_str(&json).unwrap();
        assert_eq!(back.id, "call-1");
        assert_eq!(back.provider, LiveAudioProvider::Gemini);
        assert_eq!(back.timeout_secs, 120);
        assert_eq!(back.response_schema.fields.len(), 2);
    }

    #[test]
    fn field_type_string_with_constraints() {
        let ft = FieldType::String {
            max_length: Some(50),
            allowed_values: Some(vec!["yes".into(), "no".into()]),
            tainted: true,
        };
        let json = serde_json::to_string(&ft).unwrap();
        let back: FieldType = serde_json::from_str(&json).unwrap();
        match back {
            FieldType::String {
                max_length,
                allowed_values,
                tainted,
            } => {
                assert_eq!(max_length, Some(50));
                assert_eq!(allowed_values, Some(vec!["yes".to_string(), "no".to_string()]));
                assert!(tainted);
            }
            _ => panic!("expected String variant"),
        }
    }

    #[test]
    fn field_type_integer_with_range() {
        let ft = FieldType::Integer {
            min: Some(0),
            max: Some(100),
        };
        let json = serde_json::to_string(&ft).unwrap();
        let back: FieldType = serde_json::from_str(&json).unwrap();
        match back {
            FieldType::Integer { min, max } => {
                assert_eq!(min, Some(0));
                assert_eq!(max, Some(100));
            }
            _ => panic!("expected Integer variant"),
        }
    }

    #[test]
    fn field_type_array() {
        let ft = FieldType::Array {
            element_type: Box::new(FieldType::String {
                max_length: Some(100),
                allowed_values: None,
                tainted: true,
            }),
            max_items: Some(10),
        };
        let json = serde_json::to_string(&ft).unwrap();
        let back: FieldType = serde_json::from_str(&json).unwrap();
        match back {
            FieldType::Array {
                element_type,
                max_items,
            } => {
                assert_eq!(max_items, Some(10));
                match *element_type {
                    FieldType::String { tainted, .. } => assert!(tainted),
                    _ => panic!("expected String element"),
                }
            }
            _ => panic!("expected Array variant"),
        }
    }

    #[test]
    fn live_audio_status_serde() {
        let status = LiveAudioStatus::SchemaError("missing field: confirmed".into());
        let json = serde_json::to_string(&status).unwrap();
        let back: LiveAudioStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(back, status);

        let completed = LiveAudioStatus::Completed;
        let json = serde_json::to_string(&completed).unwrap();
        let back: LiveAudioStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(back, LiveAudioStatus::Completed);
    }

    #[test]
    fn live_audio_result_serde() {
        let result = LiveAudioResult {
            id: "call-1".to_string(),
            status: LiveAudioStatus::Completed,
            response_data: Some(serde_json::json!({"confirmed": true})),
            quarantine_ids: vec!["q-001".into()],
            transcript_path: PathBuf::from("/tmp/transcript.jsonl"),
            duration_secs: 45.2,
        };
        let json = serde_json::to_string(&result).unwrap();
        let back: LiveAudioResult = serde_json::from_str(&json).unwrap();
        assert_eq!(back.id, "call-1");
        assert_eq!(back.quarantine_ids.len(), 1);
    }

    #[test]
    fn quarantine_payload_has_no_content_field() {
        let payload = QuarantinePayload {
            payload_id: "q-001".to_string(),
            timestamp: "2026-03-23T12:00:00Z".to_string(),
            live_audio_id: "call-1".to_string(),
            content_type: "tool_call_attempt".to_string(),
            summary: "unexpected tool call: browse_url".to_string(),
        };
        let json = serde_json::to_string(&payload).unwrap();
        // Verify the serialized form has no "content" field
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert!(parsed.get("content").is_none());
        assert!(parsed.get("payload_id").is_some());
        assert!(parsed.get("summary").is_some());
    }

    #[test]
    fn default_timeout_is_300() {
        let json = r#"{
            "id": "test",
            "provider": "gemini",
            "playbook": "test playbook",
            "response_schema": {"fields": []}
        }"#;
        let spec: LiveAudioSpec = serde_json::from_str(json).unwrap();
        assert_eq!(spec.timeout_secs, 300);
    }
}
