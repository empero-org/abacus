//! A small JSON Schema subset, enough to hold a delegated worker to a shape.
//!
//! Typed delegation fixes a brittle failure mode. When a worker signals its
//! answer by convention in prose — a tag, a heading, a closing paragraph — the
//! parent has to guess which part is the conclusion, and a model is not
//! reliable at separating its own answer from its intermediate reasoning. A
//! declared schema takes that judgement out of the prose entirely: either the
//! value validates or it does not, and the failure is a message the worker can
//! act on.
//!
//! This validates a deliberate subset: `type`, `required`, `properties`,
//! `items`, and `enum`. It is not a conformant JSON Schema implementation and
//! does not pretend to be — no `$ref`, no composition keywords, no format
//! assertions. Unknown keywords are ignored rather than rejected, so a richer
//! schema still works as far as this understands it. The point is to catch the
//! mistakes a model actually makes: a missing field, a string where a number
//! belongs, an object where an array belongs.

use anyhow::{Result, bail};
use serde_json::Value;

/// Check `value` against `schema`, naming the offending path on failure.
pub fn validate(value: &Value, schema: &Value) -> Result<()> {
    check(value, schema, "$")
}

fn check(value: &Value, schema: &Value, path: &str) -> Result<()> {
    let Some(schema) = schema.as_object() else {
        // A non-object schema constrains nothing; treat it as permissive
        // rather than failing a worker for the caller's malformed schema.
        return Ok(());
    };

    if let Some(expected) = schema.get("type").and_then(Value::as_str)
        && !type_matches(value, expected)
    {
        bail!("{path}: expected {expected}, found {}", type_name(value));
    }

    if let Some(allowed) = schema.get("enum").and_then(Value::as_array)
        && !allowed.contains(value)
    {
        bail!(
            "{path}: {} is not one of {}",
            value,
            Value::Array(allowed.clone())
        );
    }

    if let Some(object) = value.as_object() {
        for required in schema
            .get("required")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
        {
            if !object.contains_key(required) {
                bail!("{path}: missing required property `{required}`");
            }
        }
        if let Some(properties) = schema.get("properties").and_then(Value::as_object) {
            for (key, subschema) in properties {
                if let Some(child) = object.get(key) {
                    check(child, subschema, &format!("{path}.{key}"))?;
                }
            }
        }
    }

    if let Some(array) = value.as_array()
        && let Some(items) = schema.get("items")
    {
        for (index, child) in array.iter().enumerate() {
            check(child, items, &format!("{path}[{index}]"))?;
        }
    }

    Ok(())
}

fn type_matches(value: &Value, expected: &str) -> bool {
    match expected {
        "object" => value.is_object(),
        "array" => value.is_array(),
        "string" => value.is_string(),
        // JSON has one number type; an integer schema additionally demands no
        // fractional part, which is the distinction callers actually mean.
        "integer" => value.as_f64().is_some_and(|number| number.fract() == 0.0),
        "number" => value.is_number(),
        "boolean" => value.is_boolean(),
        "null" => value.is_null(),
        // An unknown type keyword constrains nothing rather than failing.
        _ => true,
    }
}

fn type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn findings_schema() -> Value {
        json!({
            "type": "object",
            "required": ["verdict", "findings"],
            "properties": {
                "verdict": {"type": "string", "enum": ["pass", "fail"]},
                "score": {"type": "integer"},
                "findings": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "required": ["file"],
                        "properties": {"file": {"type": "string"}}
                    }
                }
            }
        })
    }

    #[test]
    fn a_conforming_value_passes() {
        let value = json!({
            "verdict": "pass",
            "score": 3,
            "findings": [{"file": "src/main.rs"}]
        });
        validate(&value, &findings_schema()).unwrap();
    }

    #[test]
    fn a_missing_required_property_names_itself() {
        let error = validate(&json!({"verdict": "pass"}), &findings_schema())
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("missing required property `findings`"),
            "{error}"
        );
    }

    #[test]
    fn a_wrong_type_names_the_path_so_the_worker_can_fix_it() {
        let value = json!({"verdict": "pass", "findings": "not an array"});
        let error = validate(&value, &findings_schema())
            .unwrap_err()
            .to_string();
        assert!(error.contains("$.findings"), "{error}");
        assert!(error.contains("expected array, found string"), "{error}");
    }

    #[test]
    fn nested_failures_report_their_index() {
        let value = json!({
            "verdict": "pass",
            "findings": [{"file": "a.rs"}, {"line": 4}]
        });
        let error = validate(&value, &findings_schema())
            .unwrap_err()
            .to_string();
        assert!(error.contains("$.findings[1]"), "{error}");
        assert!(error.contains("`file`"), "{error}");
    }

    #[test]
    fn an_enum_rejects_an_unlisted_value() {
        let value = json!({"verdict": "maybe", "findings": []});
        let error = validate(&value, &findings_schema())
            .unwrap_err()
            .to_string();
        assert!(error.contains("not one of"), "{error}");
    }

    #[test]
    fn integer_rejects_a_fractional_number_but_number_accepts_it() {
        assert!(validate(&json!(1.5), &json!({"type": "integer"})).is_err());
        assert!(validate(&json!(2.0), &json!({"type": "integer"})).is_ok());
        assert!(validate(&json!(1.5), &json!({"type": "number"})).is_ok());
    }

    #[test]
    fn unknown_keywords_and_schemas_are_permissive_not_fatal() {
        // A caller's richer schema should still let a good value through
        // rather than failing the worker for keywords this does not implement.
        let schema = json!({
            "type": "object",
            "properties": {"a": {"type": "string", "minLength": 3, "format": "email"}},
            "allOf": [{"type": "object"}]
        });
        validate(&json!({"a": "x"}), &schema).unwrap();
        validate(&json!({"anything": true}), &json!("not a schema")).unwrap();
    }

    #[test]
    fn absent_optional_properties_are_not_checked() {
        let value = json!({"verdict": "fail", "findings": []});
        validate(&value, &findings_schema()).unwrap();
    }
}
