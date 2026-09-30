//! Maintains edit-specific Tool state and projections.
use serde_json::Value;
use similar::{ChangeTag, TextDiff};

use crate::{model::ToolCall, Error, Result};

use crate::cursor::protocol::proto::agent::v1 as pb;

#[derive(Clone, Debug)]
pub(crate) struct EditWrite {
    pub before: String,
    pub after: String,
}

pub(crate) fn path(call: &ToolCall) -> Result<String> {
    if normalized(&call.name) == "editnotebook" {
        string(call, "target_notebook")
    } else {
        string(call, "path")
    }
}

pub(crate) fn execution_path(call: &ToolCall) -> Result<Option<String>> {
    match normalized(&call.name).as_str() {
        "write" | "strreplace" | "editnotebook" => path(call).map(Some),
        _ => Ok(None),
    }
}

pub(crate) fn after_read(
    call: &ToolCall,
    result: &pb::ReadResult,
) -> std::result::Result<EditWrite, String> {
    let before = match result.result.as_ref() {
        Some(pb::read_result::Result::Success(success)) => {
            if success.truncated {
                return Err("cannot edit a truncated Read result".into());
            }
            match success.output.as_ref() {
                Some(pb::read_success::Output::Content(content)) => content.clone(),
                Some(pb::read_success::Output::Data(_)) => {
                    return Err("cannot edit a binary file".into());
                }
                None => return Err("Read result has no file content".into()),
            }
        }
        Some(pb::read_result::Result::FileNotFound(_)) if normalized(&call.name) == "write" => {
            String::new()
        }
        Some(pb::read_result::Result::FileNotFound(_)) => {
            return Err("file not found".into());
        }
        Some(pb::read_result::Result::Error(value)) => return Err(value.error.clone()),
        Some(pb::read_result::Result::Rejected(value)) => return Err(value.reason.clone()),
        Some(pb::read_result::Result::PermissionDenied(_)) => {
            return Err("read permission denied".into());
        }
        Some(pb::read_result::Result::InvalidFile(value)) => {
            return Err(value.reason.clone());
        }
        None => return Err("Read result is empty".into()),
    };
    let before = if normalized(&call.name) == "strreplace" {
        // Cursor exposes text reads as logical text, with CRLF and lone CR
        // normalized to LF. Keep the server-side edit contract at that same boundary.
        normalize_newlines(&before)
    } else {
        before
    };
    let after = match normalized(&call.name).as_str() {
        "write" => string(call, "contents").map_err(|error| error.to_string())?,
        "strreplace" => replace_string(call, &before)?,
        "editnotebook" => edit_notebook(call, &before)?,
        _ => return Err(format!("{} is not an edit tool", call.name)),
    };
    Ok(EditWrite { before, after })
}

pub(crate) fn success(path: String, write: &EditWrite) -> pb::EditResult {
    let diff = TextDiff::from_lines(&write.before, &write.after);
    let (mut added, mut removed) = (0, 0);
    for change in diff.iter_all_changes() {
        match change.tag() {
            ChangeTag::Delete => removed += 1,
            ChangeTag::Insert => added += 1,
            ChangeTag::Equal => {}
        }
    }
    pb::EditResult {
        result: Some(pb::edit_result::Result::Success(pb::EditSuccess {
            path,
            lines_added: Some(added),
            lines_removed: Some(removed),
            diff_string: Some(diff.unified_diff().to_string()),
            before_full_file_content: Some(write.before.clone()),
            after_full_file_content: write.after.clone(),
            message: None,
        })),
    }
}

pub(crate) fn failure(path: String, error: impl Into<String>) -> pb::EditResult {
    let error = error.into();
    pb::EditResult {
        result: Some(pb::edit_result::Result::Error(pb::EditError {
            path,
            error: error.clone(),
            model_visible_error: Some(error),
        })),
    }
}

pub(crate) fn normalize_newlines(value: &str) -> String {
    if !value.contains('\r') {
        return value.to_string();
    }
    // Single pass: CRLF and lone CR both become LF, matching Cursor's logical-text reads.
    let mut normalized = String::with_capacity(value.len());
    let mut rest = value;
    while let Some(index) = rest.find('\r') {
        normalized.push_str(&rest[..index]);
        if rest[index + 1..].starts_with('\n') {
            normalized.push('\n');
            rest = &rest[index + 2..];
        } else {
            normalized.push('\n');
            rest = &rest[index + 1..];
        }
    }
    normalized.push_str(rest);
    normalized
}

/// StrReplace 的 old/new 在 LF 归一后不得相同;校验层与执行层共用同一规则。
pub(crate) fn replacement_changes_text(old: &str, new: &str) -> bool {
    normalize_newlines(old) != normalize_newlines(new)
}

fn replace_string(call: &ToolCall, before: &str) -> std::result::Result<String, String> {
    let old = string(call, "old_string").map_err(|error| error.to_string())?;
    let new = string(call, "new_string").map_err(|error| error.to_string())?;
    if !replacement_changes_text(&old, &new) {
        return Err("new_string must differ from old_string".into());
    }
    let old = normalize_newlines(&old);
    let new = normalize_newlines(&new);
    if old.is_empty() {
        return Err("old_string must not be empty".into());
    }
    let occurrences = before.match_indices(&old).count();
    let replace_all = call
        .arguments
        .get("replace_all")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    match (replace_all, occurrences) {
        (_, 0) => Err("old_string was not found".into()),
        (false, 1) => Ok(before.replacen(&old, &new, 1)),
        (false, count) => Err(format!(
            "old_string is not unique; found {count} occurrences"
        )),
        (true, _) => Ok(before.replace(&old, &new)),
    }
}

fn edit_notebook(call: &ToolCall, before: &str) -> std::result::Result<String, String> {
    let mut notebook: Value =
        serde_json::from_str(before).map_err(|error| format!("invalid notebook JSON: {error}"))?;
    let cells = notebook
        .get_mut("cells")
        .and_then(Value::as_array_mut)
        .ok_or_else(|| "notebook has no cells array".to_string())?;
    let index = call
        .arguments
        .get("cell_idx")
        .and_then(Value::as_u64)
        .and_then(|value| usize::try_from(value).ok())
        .ok_or_else(|| "EditNotebook is missing cell_idx".to_string())?;
    let new = normalize_newlines(&string(call, "new_string").map_err(|error| error.to_string())?);
    if call
        .arguments
        .get("is_new_cell")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        if index > cells.len() {
            return Err(format!("cell_idx {index} is past the end of the notebook"));
        }
        let language = string(call, "cell_language").map_err(|error| error.to_string())?;
        let cell_type = if language == "markdown" || language == "raw" {
            language.as_str()
        } else {
            "code"
        };
        let mut cell = serde_json::json!({
            "cell_type": cell_type,
            "metadata": {"vscode": {"languageId": language}},
            "source": source_lines(&new),
        });
        if cell_type == "code" {
            cell["execution_count"] = Value::Null;
            cell["outputs"] = Value::Array(Vec::new());
        }
        cells.insert(index, cell);
    } else {
        let cell = cells
            .get_mut(index)
            .ok_or_else(|| format!("cell_idx {index} does not exist"))?;
        let source = cell
            .get("source")
            .map(notebook_source)
            .transpose()?
            .unwrap_or_default();
        let old =
            normalize_newlines(&string(call, "old_string").map_err(|error| error.to_string())?);
        if old.is_empty() {
            return Err("old_string must not be empty".into());
        }
        let occurrences = source.match_indices(&old).count();
        let edited = match occurrences {
            0 => return Err("old_string was not found in the notebook cell".into()),
            1 => source.replacen(&old, &new, 1),
            count => {
                return Err(format!(
                    "old_string is not unique in the notebook cell; found {count} occurrences"
                ))
            }
        };
        cell["source"] = Value::Array(source_lines(&edited));
    }
    serde_json::to_string_pretty(&notebook)
        .map(|value| format!("{value}\n"))
        .map_err(|error| error.to_string())
}

fn notebook_source(value: &Value) -> std::result::Result<String, String> {
    match value {
        Value::String(value) => Ok(normalize_newlines(value)),
        Value::Array(lines) => lines
            .iter()
            .map(|line| {
                line.as_str()
                    .ok_or_else(|| "notebook cell source contains a non-string".to_string())
            })
            .collect::<std::result::Result<Vec<_>, _>>()
            .map(|lines| normalize_newlines(&lines.concat())),
        _ => Err("notebook cell source is not text".into()),
    }
}

fn source_lines(value: &str) -> Vec<Value> {
    if value.is_empty() {
        Vec::new()
    } else {
        value
            .split_inclusive('\n')
            .map(|line| Value::String(line.to_string()))
            .collect()
    }
}

fn string(call: &ToolCall, field: &str) -> Result<String> {
    call.arguments
        .get(field)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| Error::Protocol(format!("{} is missing {field}", call.name)))
}

fn normalized(value: &str) -> String {
    value
        .chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

#[cfg(test)]
mod tests {
    use serde_json::{json, Value};

    use super::{after_read, edit_notebook, path, pb};
    use crate::model::ToolCall;

    fn notebook_call(old_string: &str) -> ToolCall {
        ToolCall {
            index: 0,
            call_id: "call".into(),
            model_call_id: "model".into(),
            name: "EditNotebook".into(),
            arguments_text: String::new(),
            arguments: json!({
                "target_notebook": "/notebook.ipynb",
                "cell_idx": 0,
                "old_string": old_string,
                "new_string": "replacement",
            }),
            argument_error: None,
        }
    }

    fn single_cell_notebook() -> String {
        json!({
            "cells": [{"cell_type": "code", "source": ["print('hi')\n"]}],
        })
        .to_string()
    }

    fn read_text(text: &str) -> pb::ReadResult {
        pb::ReadResult {
            result: Some(pb::read_result::Result::Success(pb::ReadSuccess {
                output: Some(pb::read_success::Output::Content(text.into())),
                ..Default::default()
            })),
        }
    }

    #[test]
    fn write_preserves_input_and_original_line_endings() {
        let mut call = notebook_call("unused");
        call.name = "Write".into();
        for contents in ["", "a\r\nb\rc\n", "a\n", "no final newline"] {
            call.arguments = json!({"path": "/test", "contents": contents});
            let edited = after_read(&call, &read_text("before\r\n")).unwrap();
            assert_eq!(edited.before, "before\r\n");
            assert_eq!(edited.after, contents);
            let rendered = crate::cursor::tools::codec::render_tool_call(&call, false).unwrap();
            let Some(pb::tool_call::Tool::EditToolCall(tool)) = rendered.tool else {
                panic!("expected edit card")
            };
            let args = tool.args.unwrap();
            assert_eq!(args.path, "/test");
            assert_eq!(args.stream_content.as_deref(), Some(contents));
        }
        call.arguments = json!({"file_path": "/alias", "content": "alias"});
        assert!(path(&call).is_err());
        assert!(after_read(&call, &read_text("")).is_err());
        for value in [Value::Null, json!(42), json!([])] {
            call.arguments = json!({"path": value, "contents": value});
            assert!(path(&call).is_err());
            assert!(after_read(&call, &read_text("")).is_err());
        }
    }

    #[test]
    fn replacement_normalizes_read_match_and_written_text_to_lf() {
        let mut call = notebook_call("before");
        call.name = "StrReplace".into();
        for (before, old, new, normalized_before, after) in [
            ("a\nb\n", "a\nb", "x", "a\nb\n", "x\n"),
            (
                "keep\r\ntarget\nline\rfooter",
                "target\rline",
                "new\r\nline\rend",
                "keep\ntarget\nline\nfooter",
                "keep\nnew\nline\nend\nfooter",
            ),
        ] {
            call.arguments = json!({"path": "/test", "old_string": old, "new_string": new});
            let edited = after_read(&call, &read_text(before)).unwrap();
            assert_eq!(edited.before, normalized_before);
            assert_eq!(edited.after, after);
        }
    }

    #[test]
    fn replacement_remains_literal_and_counts_logical_occurrences() {
        let mut call = notebook_call("before");
        call.name = "StrReplace".into();
        call.arguments = json!({
            "path": "/test",
            "old_string": "[before]\r\n",
            "new_string": "after\r",
        });
        let before = "keep [before]\r\nkeep\n[before]\rkeep";
        let error = after_read(&call, &read_text(before)).unwrap_err();
        assert_eq!(error, "old_string is not unique; found 2 occurrences");

        call.arguments["replace_all"] = json!(true);
        let edited = after_read(&call, &read_text(before)).unwrap();
        assert_eq!(edited.before, "keep [before]\nkeep\n[before]\nkeep");
        assert_eq!(edited.after, "keep after\nkeep\nafter\nkeep");

        call.arguments = json!({"path": "/test", "old_string": "same\r\n", "new_string": "same\n"});
        assert_eq!(
            after_read(&call, &read_text("same\n")).unwrap_err(),
            "new_string must differ from old_string"
        );
        call.arguments = json!({"path": "/test", "old_string": "", "new_string": "x"});
        assert_eq!(
            after_read(&call, &read_text("same\n")).unwrap_err(),
            "old_string must not be empty"
        );
        call.arguments = json!({"path": "/test", "old_string": "missing", "new_string": "x"});
        assert_eq!(
            after_read(&call, &read_text("same\n")).unwrap_err(),
            "old_string was not found"
        );
    }

    #[test]
    fn edit_notebook_rejects_empty_old_string() {
        // StrReplace rejects an empty old_string; EditNotebook must do the same
        // instead of prepending new_string (empty cell) or reporting a
        // misleading "not unique" error (non-empty cell).
        let error = edit_notebook(&notebook_call(""), &single_cell_notebook()).unwrap_err();
        assert_eq!(error, "old_string must not be empty");
    }

    #[test]
    fn edit_notebook_inserts_cell_with_language_metadata() {
        let mut call = notebook_call("");
        call.arguments = json!({
            "target_notebook": "/notebook.ipynb",
            "cell_idx": 1,
            "new_string": "console.log('hi')\n",
            "is_new_cell": true,
            "cell_language": "typescript",
        });
        let before = json!({
            "cells": [{"cell_type": "markdown", "metadata": {}, "source": ["# Title\n"]}],
        })
        .to_string();

        let edited: Value = serde_json::from_str(&edit_notebook(&call, &before).unwrap()).unwrap();
        let inserted = &edited["cells"][1];
        assert_eq!(inserted["cell_type"], "code");
        assert_eq!(inserted["metadata"]["vscode"]["languageId"], "typescript");
        assert_eq!(inserted["source"], json!(["console.log('hi')\n"]));
        assert!(inserted["execution_count"].is_null());
        assert_eq!(inserted["outputs"], json!([]));
    }

    #[test]
    fn edit_notebook_replaces_a_unique_old_string() {
        let edited = edit_notebook(&notebook_call("hi"), &single_cell_notebook()).unwrap();
        assert!(edited.contains("print('replacement')"));
    }
}
