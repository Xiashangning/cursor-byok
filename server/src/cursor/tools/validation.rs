//! Validates original model arguments against the definitions actually advertised for this run.
use crate::{
    model::{ToolCall, ToolDefinition},
    Error, Result,
};

pub(crate) fn validate(call: &ToolCall, definitions: &[ToolDefinition]) -> Result<()> {
    let definition = definitions
        .iter()
        .find(|definition| definition.name == call.name)
        .ok_or_else(|| {
            Error::Protocol(format!(
                "Tool {} is not in the current advertised tool set",
                call.name
            ))
        })?;
    // Remote/file reference resolution is disabled at the dependency level. Validating a
    // descriptor must never initiate network or filesystem access.
    let validator = jsonschema::validator_for(&definition.parameters)
        .map_err(|error| Error::Protocol(format!("Invalid schema for {}: {error}", call.name)))?;
    validator
        .validate(&call.arguments)
        .map_err(|error| Error::Protocol(format!("Invalid {} arguments: {error}", call.name)))
}

// JSON Schema integer accepts mathematical integers such as 2.0. Static wire
// encoders and serde integer fields need their integer JSON representation.
// This only changes the execution copy, never canonical history or MCP inputs.
pub(crate) fn normalize_integers(value: &mut serde_json::Value) {
    use serde_json::Value;
    match value {
        Value::Array(values) => values.iter_mut().for_each(normalize_integers),
        Value::Object(values) => values.values_mut().for_each(normalize_integers),
        Value::Number(number) if number.is_f64() => {
            if let Some(number) = number.as_f64().filter(|number| {
                number.fract() == 0.0 && *number >= i64::MIN as f64 && *number < i64::MAX as f64
            }) {
                *value = Value::from(number as i64);
            }
        }
        _ => {}
    }
}

pub(crate) fn mcp_arguments(call: &ToolCall, context: &super::runtime::ExecContext) -> Result<()> {
    if call.name != "CallMcpTool" {
        return Ok(());
    }
    let server = call.arguments["server"].as_str().unwrap_or_default();
    let tool = call.arguments["toolName"].as_str().unwrap_or_default();
    let Some(schema) = context
        .mcp_routes
        .get(&(server.into(), tool.into()))
        .and_then(|route| route.input_schema.as_deref())
    else {
        // Path-only descriptors belong to the client's filesystem, not the server's.
        return Ok(());
    };
    let schema: serde_json::Value = serde_json::from_str(schema)?;
    let arguments = call
        .arguments
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| serde_json::json!({}));
    let validator = jsonschema::validator_for(&schema)
        .map_err(|error| Error::Protocol(format!("Invalid MCP schema: {error}")))?;
    validator.validate(&arguments).map_err(|error| {
        Error::Protocol(format!(
            "Invalid MCP arguments for {server}/{tool}: {error}"
        ))
    })
}

pub(crate) fn semantics(call: &ToolCall) -> Result<()> {
    if call.name == "GetMcpTools" {
        if let Some(pattern) = call
            .arguments
            .get("pattern")
            .and_then(serde_json::Value::as_str)
        {
            regex::Regex::new(pattern).map_err(|error| {
                Error::Protocol(format!("GetMcpTools invalid Rust regex: {error}"))
            })?;
        }
    }
    if call.name == "StrReplace" {
        let old = call
            .arguments
            .get("old_string")
            .and_then(serde_json::Value::as_str);
        let new = call
            .arguments
            .get("new_string")
            .and_then(serde_json::Value::as_str);
        if old
            .zip(new)
            .is_some_and(|(old, new)| !super::edit::replacement_changes_text(old, new))
        {
            return Err(Error::Protocol(
                "new_string must differ from old_string".into(),
            ));
        }
    }
    Ok(())
}
