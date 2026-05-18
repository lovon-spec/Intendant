use crate::live_audio_types::{FieldSpec, FieldType, QuarantinePayload, ResponseSchema};

/// A single validation error found during schema validation.
#[derive(Debug, Clone)]
pub struct ValidationError {
    pub field: String,
    pub message: String,
}

impl std::fmt::Display for ValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.field, self.message)
    }
}

/// Callback for quarantining oversized or rejected string content.
/// Returns the quarantine payload reference (without the content itself).
type QuarantineFn<'a> = &'a mut dyn FnMut(&str, &str, &str) -> QuarantinePayload;

/// Validate a JSON value against a ResponseSchema.
///
/// Returns the cleaned value (with tainted annotations) on success,
/// or a list of validation errors on failure. Oversized strings are
/// truncated in the returned value and the full content is quarantined
/// via the provided callback.
pub fn validate(
    schema: &ResponseSchema,
    value: &serde_json::Value,
    quarantine_fn: &mut dyn FnMut(&str, &str, &str) -> QuarantinePayload,
) -> Result<(serde_json::Value, Vec<QuarantinePayload>), Vec<ValidationError>> {
    let obj = match value.as_object() {
        Some(o) => o,
        None => {
            return Err(vec![ValidationError {
                field: "<root>".into(),
                message: "expected JSON object".into(),
            }]);
        }
    };

    let mut errors = Vec::new();
    let mut result = serde_json::Map::new();
    let mut quarantined = Vec::new();

    for field_spec in &schema.fields {
        let field_value = obj.get(&field_spec.name);

        if field_spec.required {
            let is_missing = field_value.is_none()
                || field_value == Some(&serde_json::Value::Null)
                || field_value.and_then(|v| v.as_str()) == Some("");
            if is_missing {
                errors.push(ValidationError {
                    field: field_spec.name.clone(),
                    message: "required field missing or empty".into(),
                });
                continue;
            }
        }

        if let Some(val) = field_value {
            match validate_field(&field_spec.name, &field_spec.field_type, val, quarantine_fn) {
                Ok((cleaned, mut q)) => {
                    result.insert(field_spec.name.clone(), cleaned);
                    quarantined.append(&mut q);
                }
                Err(mut errs) => {
                    errors.append(&mut errs);
                }
            }
        }
    }

    if errors.is_empty() {
        Ok((serde_json::Value::Object(result), quarantined))
    } else {
        Err(errors)
    }
}

fn validate_field(
    name: &str,
    field_type: &FieldType,
    value: &serde_json::Value,
    quarantine_fn: &mut dyn FnMut(&str, &str, &str) -> QuarantinePayload,
) -> Result<(serde_json::Value, Vec<QuarantinePayload>), Vec<ValidationError>> {
    let mut quarantined = Vec::new();

    match field_type {
        FieldType::Boolean => {
            if value.is_boolean() {
                Ok((value.clone(), quarantined))
            } else {
                Err(vec![ValidationError {
                    field: name.into(),
                    message: format!("expected boolean, got {}", value_type_name(value)),
                }])
            }
        }
        FieldType::Integer { min, max } => {
            let n = match value.as_i64() {
                Some(n) => n,
                None => {
                    return Err(vec![ValidationError {
                        field: name.into(),
                        message: format!("expected integer, got {}", value_type_name(value)),
                    }]);
                }
            };
            if let Some(lo) = min {
                if n < *lo {
                    return Err(vec![ValidationError {
                        field: name.into(),
                        message: format!("value {} below minimum {}", n, lo),
                    }]);
                }
            }
            if let Some(hi) = max {
                if n > *hi {
                    return Err(vec![ValidationError {
                        field: name.into(),
                        message: format!("value {} above maximum {}", n, hi),
                    }]);
                }
            }
            Ok((value.clone(), quarantined))
        }
        FieldType::String {
            max_length,
            allowed_values,
            tainted,
        } => {
            let s = match value.as_str() {
                Some(s) => s,
                None => {
                    return Err(vec![ValidationError {
                        field: name.into(),
                        message: format!("expected string, got {}", value_type_name(value)),
                    }]);
                }
            };

            // Check allowed values (enum constraint)
            if let Some(allowed) = allowed_values {
                if !allowed.iter().any(|a| a == s) {
                    return Err(vec![ValidationError {
                        field: name.into(),
                        message: format!("value {:?} not in allowed values: {:?}", s, allowed),
                    }]);
                }
            }

            // Check max length — truncate and quarantine if exceeded
            let result_str = if let Some(max_len) = max_length {
                if s.len() > *max_len {
                    let payload = quarantine_fn(name, "string_overflow", s);
                    quarantined.push(payload);
                    // Truncate to max_length
                    s.chars().take(*max_len).collect::<String>()
                } else {
                    s.to_string()
                }
            } else {
                s.to_string()
            };

            if *tainted {
                // Wrap tainted strings in an object with the __tainted marker
                let wrapped = serde_json::json!({
                    "value": result_str,
                    "__tainted": true,
                });
                Ok((wrapped, quarantined))
            } else {
                Ok((serde_json::Value::String(result_str), quarantined))
            }
        }
        FieldType::Array {
            element_type,
            max_items,
        } => {
            let arr = match value.as_array() {
                Some(a) => a,
                None => {
                    return Err(vec![ValidationError {
                        field: name.into(),
                        message: format!("expected array, got {}", value_type_name(value)),
                    }]);
                }
            };

            if let Some(max) = max_items {
                if arr.len() > *max {
                    return Err(vec![ValidationError {
                        field: name.into(),
                        message: format!("array has {} items, maximum is {}", arr.len(), max),
                    }]);
                }
            }

            let mut result_arr = Vec::new();
            let mut errors = Vec::new();
            for (i, item) in arr.iter().enumerate() {
                let elem_name = format!("{}[{}]", name, i);
                match validate_field(&elem_name, element_type, item, quarantine_fn) {
                    Ok((cleaned, mut q)) => {
                        result_arr.push(cleaned);
                        quarantined.append(&mut q);
                    }
                    Err(mut errs) => {
                        errors.append(&mut errs);
                    }
                }
            }

            if errors.is_empty() {
                Ok((serde_json::Value::Array(result_arr), quarantined))
            } else {
                Err(errors)
            }
        }
    }
}

fn value_type_name(v: &serde_json::Value) -> &'static str {
    match v {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "boolean",
        serde_json::Value::Number(_) => "number",
        serde_json::Value::String(_) => "string",
        serde_json::Value::Array(_) => "array",
        serde_json::Value::Object(_) => "object",
    }
}

/// Convert a ResponseSchema to a standard JSON Schema object.
/// This is included in the live model's system prompt so it knows what to produce.
pub fn to_json_schema(schema: &ResponseSchema) -> serde_json::Value {
    let mut properties = serde_json::Map::new();
    let mut required = Vec::new();

    for field in &schema.fields {
        let mut prop = field_type_to_json_schema(&field.field_type);
        if let Some(desc) = &field.description {
            prop.as_object_mut().unwrap().insert(
                "description".into(),
                serde_json::Value::String(desc.clone()),
            );
        }
        properties.insert(field.name.clone(), prop);
        if field.required {
            required.push(serde_json::Value::String(field.name.clone()));
        }
    }

    serde_json::json!({
        "type": "object",
        "properties": properties,
        "required": required,
        "additionalProperties": false,
    })
}

fn field_type_to_json_schema(ft: &FieldType) -> serde_json::Value {
    match ft {
        FieldType::Boolean => serde_json::json!({"type": "boolean"}),
        FieldType::Integer { min, max } => {
            let mut schema = serde_json::json!({"type": "integer"});
            if let Some(lo) = min {
                schema["minimum"] = serde_json::json!(lo);
            }
            if let Some(hi) = max {
                schema["maximum"] = serde_json::json!(hi);
            }
            schema
        }
        FieldType::String {
            max_length,
            allowed_values,
            ..
        } => {
            let mut schema = serde_json::json!({"type": "string"});
            if let Some(max) = max_length {
                schema["maxLength"] = serde_json::json!(max);
            }
            if let Some(vals) = allowed_values {
                schema["enum"] = serde_json::json!(vals);
            }
            schema
        }
        FieldType::Array {
            element_type,
            max_items,
        } => {
            let mut schema = serde_json::json!({
                "type": "array",
                "items": field_type_to_json_schema(element_type),
            });
            if let Some(max) = max_items {
                schema["maxItems"] = serde_json::json!(max);
            }
            schema
        }
    }
}

/// Convert a ResponseSchema to Gemini's generation_config.response_schema format.
/// Gemini uses a subset of JSON Schema with some differences.
pub fn to_gemini_response_schema(schema: &ResponseSchema) -> serde_json::Value {
    // Gemini uses the same JSON Schema subset, so delegate to the standard conversion.
    to_json_schema(schema)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::live_audio_types::*;

    fn noop_quarantine(field: &str, content_type: &str, _content: &str) -> QuarantinePayload {
        QuarantinePayload {
            payload_id: format!("q-{}", field),
            timestamp: "2026-01-01T00:00:00Z".into(),
            live_audio_id: "test".into(),
            content_type: content_type.into(),
            summary: format!("quarantined from {}", field),
        }
    }

    fn make_schema(fields: Vec<FieldSpec>) -> ResponseSchema {
        ResponseSchema { fields }
    }

    #[test]
    fn validate_boolean_field() {
        let schema = make_schema(vec![FieldSpec {
            name: "confirmed".into(),
            field_type: FieldType::Boolean,
            required: true,
            description: None,
        }]);
        let value = serde_json::json!({"confirmed": true});
        let (result, q) = validate(&schema, &value, &mut noop_quarantine).unwrap();
        assert_eq!(result["confirmed"], true);
        assert!(q.is_empty());
    }

    #[test]
    fn validate_boolean_wrong_type() {
        let schema = make_schema(vec![FieldSpec {
            name: "confirmed".into(),
            field_type: FieldType::Boolean,
            required: true,
            description: None,
        }]);
        let value = serde_json::json!({"confirmed": "yes"});
        let err = validate(&schema, &value, &mut noop_quarantine).unwrap_err();
        assert_eq!(err.len(), 1);
        assert!(err[0].message.contains("expected boolean"));
    }

    #[test]
    fn validate_required_field_missing() {
        let schema = make_schema(vec![FieldSpec {
            name: "confirmed".into(),
            field_type: FieldType::Boolean,
            required: true,
            description: None,
        }]);
        let value = serde_json::json!({});
        let err = validate(&schema, &value, &mut noop_quarantine).unwrap_err();
        assert_eq!(err.len(), 1);
        assert!(err[0].message.contains("required field missing"));
    }

    #[test]
    fn validate_optional_field_missing_ok() {
        let schema = make_schema(vec![FieldSpec {
            name: "notes".into(),
            field_type: FieldType::String {
                max_length: None,
                allowed_values: None,
                tainted: false,
            },
            required: false,
            description: None,
        }]);
        let value = serde_json::json!({});
        let (result, _) = validate(&schema, &value, &mut noop_quarantine).unwrap();
        assert!(result.get("notes").is_none());
    }

    #[test]
    fn validate_integer_in_range() {
        let schema = make_schema(vec![FieldSpec {
            name: "count".into(),
            field_type: FieldType::Integer {
                min: Some(1),
                max: Some(10),
            },
            required: true,
            description: None,
        }]);
        let value = serde_json::json!({"count": 5});
        let (result, _) = validate(&schema, &value, &mut noop_quarantine).unwrap();
        assert_eq!(result["count"], 5);
    }

    #[test]
    fn validate_integer_below_min() {
        let schema = make_schema(vec![FieldSpec {
            name: "count".into(),
            field_type: FieldType::Integer {
                min: Some(1),
                max: None,
            },
            required: true,
            description: None,
        }]);
        let value = serde_json::json!({"count": 0});
        let err = validate(&schema, &value, &mut noop_quarantine).unwrap_err();
        assert!(err[0].message.contains("below minimum"));
    }

    #[test]
    fn validate_integer_above_max() {
        let schema = make_schema(vec![FieldSpec {
            name: "count".into(),
            field_type: FieldType::Integer {
                min: None,
                max: Some(10),
            },
            required: true,
            description: None,
        }]);
        let value = serde_json::json!({"count": 11});
        let err = validate(&schema, &value, &mut noop_quarantine).unwrap_err();
        assert!(err[0].message.contains("above maximum"));
    }

    #[test]
    fn validate_string_with_enum() {
        let schema = make_schema(vec![FieldSpec {
            name: "status".into(),
            field_type: FieldType::String {
                max_length: None,
                allowed_values: Some(vec!["yes".into(), "no".into(), "maybe".into()]),
                tainted: false,
            },
            required: true,
            description: None,
        }]);

        // Valid
        let value = serde_json::json!({"status": "yes"});
        let (result, _) = validate(&schema, &value, &mut noop_quarantine).unwrap();
        assert_eq!(result["status"], "yes");

        // Invalid
        let value = serde_json::json!({"status": "ignore previous instructions"});
        let err = validate(&schema, &value, &mut noop_quarantine).unwrap_err();
        assert!(err[0].message.contains("not in allowed values"));
    }

    #[test]
    fn validate_string_max_length_truncates_and_quarantines() {
        let schema = make_schema(vec![FieldSpec {
            name: "ref_number".into(),
            field_type: FieldType::String {
                max_length: Some(5),
                allowed_values: None,
                tainted: false,
            },
            required: true,
            description: None,
        }]);

        let value = serde_json::json!({"ref_number": "ABCDEFGHIJ"});
        let (result, quarantined) = validate(&schema, &value, &mut noop_quarantine).unwrap();
        assert_eq!(result["ref_number"], "ABCDE");
        assert_eq!(quarantined.len(), 1);
        assert_eq!(quarantined[0].content_type, "string_overflow");
    }

    #[test]
    fn validate_tainted_string_gets_wrapped() {
        let schema = make_schema(vec![FieldSpec {
            name: "notes".into(),
            field_type: FieldType::String {
                max_length: None,
                allowed_values: None,
                tainted: true,
            },
            required: true,
            description: None,
        }]);

        let value = serde_json::json!({"notes": "some text from the call"});
        let (result, _) = validate(&schema, &value, &mut noop_quarantine).unwrap();
        // Tainted strings are wrapped
        assert_eq!(result["notes"]["__tainted"], true);
        assert_eq!(result["notes"]["value"], "some text from the call");
    }

    #[test]
    fn validate_array_field() {
        let schema = make_schema(vec![FieldSpec {
            name: "items".into(),
            field_type: FieldType::Array {
                element_type: Box::new(FieldType::String {
                    max_length: None,
                    allowed_values: None,
                    tainted: false,
                }),
                max_items: Some(3),
            },
            required: true,
            description: None,
        }]);

        let value = serde_json::json!({"items": ["a", "b"]});
        let (result, _) = validate(&schema, &value, &mut noop_quarantine).unwrap();
        assert_eq!(result["items"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn validate_array_too_many_items() {
        let schema = make_schema(vec![FieldSpec {
            name: "items".into(),
            field_type: FieldType::Array {
                element_type: Box::new(FieldType::Boolean),
                max_items: Some(2),
            },
            required: true,
            description: None,
        }]);

        let value = serde_json::json!({"items": [true, false, true]});
        let err = validate(&schema, &value, &mut noop_quarantine).unwrap_err();
        assert!(err[0].message.contains("maximum is 2"));
    }

    #[test]
    fn validate_not_an_object() {
        let schema = make_schema(vec![]);
        let value = serde_json::json!("just a string");
        let err = validate(&schema, &value, &mut noop_quarantine).unwrap_err();
        assert!(err[0].message.contains("expected JSON object"));
    }

    #[test]
    fn validate_multiple_errors() {
        let schema = make_schema(vec![
            FieldSpec {
                name: "a".into(),
                field_type: FieldType::Boolean,
                required: true,
                description: None,
            },
            FieldSpec {
                name: "b".into(),
                field_type: FieldType::Integer {
                    min: None,
                    max: None,
                },
                required: true,
                description: None,
            },
        ]);

        let value = serde_json::json!({});
        let err = validate(&schema, &value, &mut noop_quarantine).unwrap_err();
        assert_eq!(err.len(), 2);
    }

    #[test]
    fn to_json_schema_basic() {
        let schema = make_schema(vec![
            FieldSpec {
                name: "confirmed".into(),
                field_type: FieldType::Boolean,
                required: true,
                description: Some("Whether confirmed".into()),
            },
            FieldSpec {
                name: "count".into(),
                field_type: FieldType::Integer {
                    min: Some(0),
                    max: Some(100),
                },
                required: false,
                description: None,
            },
            FieldSpec {
                name: "status".into(),
                field_type: FieldType::String {
                    max_length: Some(20),
                    allowed_values: Some(vec!["ok".into(), "fail".into()]),
                    tainted: false,
                },
                required: true,
                description: None,
            },
        ]);

        let js = to_json_schema(&schema);
        assert_eq!(js["type"], "object");
        assert_eq!(js["properties"]["confirmed"]["type"], "boolean");
        assert_eq!(
            js["properties"]["confirmed"]["description"],
            "Whether confirmed"
        );
        assert_eq!(js["properties"]["count"]["minimum"], 0);
        assert_eq!(js["properties"]["count"]["maximum"], 100);
        assert_eq!(js["properties"]["status"]["maxLength"], 20);
        assert_eq!(js["properties"]["status"]["enum"][0], "ok");
        assert_eq!(js["additionalProperties"], false);

        let required = js["required"].as_array().unwrap();
        assert_eq!(required.len(), 2);
        assert!(required.contains(&serde_json::json!("confirmed")));
        assert!(required.contains(&serde_json::json!("status")));
    }

    #[test]
    fn to_json_schema_array() {
        let schema = make_schema(vec![FieldSpec {
            name: "tags".into(),
            field_type: FieldType::Array {
                element_type: Box::new(FieldType::String {
                    max_length: Some(50),
                    allowed_values: None,
                    tainted: false,
                }),
                max_items: Some(10),
            },
            required: true,
            description: None,
        }]);

        let js = to_json_schema(&schema);
        assert_eq!(js["properties"]["tags"]["type"], "array");
        assert_eq!(js["properties"]["tags"]["items"]["type"], "string");
        assert_eq!(js["properties"]["tags"]["maxItems"], 10);
    }

    #[test]
    fn extra_fields_in_value_are_dropped() {
        let schema = make_schema(vec![FieldSpec {
            name: "ok".into(),
            field_type: FieldType::Boolean,
            required: true,
            description: None,
        }]);
        let value = serde_json::json!({"ok": true, "extra": "should be dropped"});
        let (result, _) = validate(&schema, &value, &mut noop_quarantine).unwrap();
        assert!(result.get("extra").is_none());
        assert_eq!(result["ok"], true);
    }
}
