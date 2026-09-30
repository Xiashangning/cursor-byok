//! Parses and persists command execution output.
use crate::{cursor::protocol::proto::agent::v1 as pb, model::ToolCall, Error, Result};

pub(super) fn output(
    message: &pb::exec_client_message::Message,
    call: &ToolCall,
) -> Result<(String, bool)> {
    use pb::exec_client_message::Message;
    match message {
        Message::ShellResult(value) | Message::MiniSweAgentBashResult(value) => shell(value),
        Message::ReadResult(value) | Message::RedactedReadResult(value) => read(value),
        Message::WriteResult(value) => write(value),
        Message::DeleteResult(value) => delete(value),
        Message::GrepResult(value) => grep(value),
        Message::DiagnosticsResult(value) => diagnostics(value),
        Message::McpResult(value) => mcp(value),
        Message::ReadMcpResourceExecResult(value) => read_mcp(value),
        Message::SubagentResult(value) => task(value, call),
        _ => Err(Error::Protocol(
            "unsupported terminal ExecClientMessage".into(),
        )),
    }
}

fn shell(value: &pb::ShellResult) -> Result<(String, bool)> {
    use pb::shell_result::Result as R;
    let output = match value.result.as_ref().ok_or_else(|| missing("shell"))? {
        R::Success(success) if value.is_background == Some(true) => {
            let mut fields = vec![format!("shell_id={}", success.shell_id.unwrap_or_default())];
            if let Some(pid) = success.pid.or(value.pid) {
                fields.push(format!("pid={pid}"));
            }
            if let Some(folder) = value.terminals_folder.as_deref().filter(|v| !v.is_empty()) {
                fields.push(format!("terminals_folder={folder}"));
            }
            let output = streams(&success.stdout, &success.stderr);
            let prefix = format!(
                "shell running in background {}\nCursor will notify you when it finishes. Outside Multitask Mode, continue independent work if possible; when none remains, you MUST call Await with this shell_id and must not end the turn. In Multitask Mode, call Await when the next step depends on this shell, or end the turn and wait for the notification.",
                fields.join(" ")
            );
            return Ok((
                if output == "shell completed without output" {
                    prefix
                } else {
                    format!("{prefix}\n{output}")
                },
                false,
            ));
        }
        R::Success(success) => return Ok((streams(&success.stdout, &success.stderr), false)),
        R::Failure(failure) => streams(&failure.stdout, &failure.stderr),
        R::Timeout(timeout) => format!(
            "shell timed out after {}ms in {}",
            timeout.timeout_ms, timeout.working_directory
        ),
        R::Rejected(rejected) => rejected.reason.clone(),
        R::SpawnError(error) => error.error.clone(),
        R::PermissionDenied(denied) => denied.error.clone(),
    };
    Ok((output, true))
}

fn streams(stdout: &str, stderr: &str) -> String {
    match (stdout.is_empty(), stderr.is_empty()) {
        (false, false) => format!("{stdout}\n\n<stderr>\n{stderr}\n</stderr>"),
        (false, true) => stdout.into(),
        (true, false) => stderr.into(),
        (true, true) => "shell completed without output".into(),
    }
}

fn read(value: &pb::ReadResult) -> Result<(String, bool)> {
    use pb::{read_result::Result as R, read_success::Output};
    match value.result.as_ref().ok_or_else(|| missing("read"))? {
        R::Success(success) => Ok((
            match success.output.as_ref() {
                Some(Output::Content(text)) => text.clone(),
                Some(Output::Data(bytes)) => format!("read binary bytes={}", bytes.len()),
                None => format!("read success path={}", success.path),
            },
            false,
        )),
        R::Error(error) => Ok((error.error.clone(), true)),
        R::Rejected(rejected) => Ok((rejected.reason.clone(), true)),
        R::FileNotFound(value) => Ok((format!("file not found: {}", value.path), true)),
        R::PermissionDenied(value) => Ok((format!("permission denied: {}", value.path), true)),
        R::InvalidFile(value) => Ok((value.reason.clone(), true)),
    }
}

fn write(value: &pb::WriteResult) -> Result<(String, bool)> {
    use pb::write_result::Result as R;
    match value.result.as_ref().ok_or_else(|| missing("write"))? {
        R::Success(success) => Ok((
            success.file_content_after_write.clone().unwrap_or_else(|| {
                format!(
                    "write success path={} lines={}",
                    success.path, success.lines_created
                )
            }),
            false,
        )),
        R::PermissionDenied(value) => Ok((value.error.clone(), true)),
        R::NoSpace(value) => Ok((format!("no space left: {}", value.path), true)),
        R::Error(value) => Ok((value.error.clone(), true)),
        R::Rejected(value) => Ok((value.reason.clone(), true)),
    }
}

fn delete(value: &pb::DeleteResult) -> Result<(String, bool)> {
    use pb::delete_result::Result as R;
    match value.result.as_ref().ok_or_else(|| missing("delete"))? {
        R::Success(value) => Ok((format!("delete success path={}", value.path), false)),
        R::FileNotFound(value) => Ok((format!("file not found: {}", value.path), true)),
        R::NotFile(value) => Ok((format!("not file: {}", value.path), true)),
        R::PermissionDenied(value) => Ok((value.client_visible_error.clone(), true)),
        R::FileBusy(value) => Ok((format!("file busy: {}", value.path), true)),
        R::Rejected(value) => Ok((value.reason.clone(), true)),
        R::Error(value) => Ok((value.error.clone(), true)),
    }
}

fn grep(value: &pb::GrepResult) -> Result<(String, bool)> {
    use pb::grep_result::Result as R;
    match value.result.as_ref().ok_or_else(|| missing("grep"))? {
        R::Success(value) => Ok((grep_success(value), false)),
        R::Error(value) => Ok((value.error.clone(), true)),
    }
}

fn grep_success(value: &pb::GrepSuccess) -> String {
    let mut lines = Vec::new();
    if let Some(result) = &value.active_editor_result {
        grep_union(result, &mut lines);
    }
    let mut workspaces = value.workspace_results.iter().collect::<Vec<_>>();
    workspaces.sort_unstable_by_key(|(name, _)| *name);
    for (_, result) in workspaces {
        grep_union(result, &mut lines);
    }
    if lines.is_empty() {
        format!(
            "No matches found for pattern `{}` in {}",
            value.pattern, value.path
        )
    } else {
        lines.join("\n")
    }
}

fn grep_union(value: &pb::GrepUnionResult, lines: &mut Vec<String>) {
    use pb::grep_union_result::Result as R;
    match value.result.as_ref() {
        Some(R::Files(value)) => {
            lines.extend(value.files.iter().cloned());
            grep_truncation(
                value.client_truncated,
                value.ripgrep_truncated,
                value.total_files,
                "files",
                lines,
            );
        }
        Some(R::Count(value)) => {
            lines.extend(
                value
                    .counts
                    .iter()
                    .map(|count| format!("{}:{}", count.file, count.count)),
            );
            grep_truncation(
                value.client_truncated,
                value.ripgrep_truncated,
                value.total_matches,
                "matches",
                lines,
            );
        }
        Some(R::Content(value)) => {
            for file in &value.matches {
                lines.extend(file.matches.iter().map(|matched| {
                    let separator = if matched.is_context_line { '-' } else { ':' };
                    let truncated = if matched.content_truncated {
                        " [line truncated]"
                    } else {
                        ""
                    };
                    format!(
                        "{}{separator}{}{separator}{}{truncated}",
                        file.file, matched.line_number, matched.content
                    )
                }));
            }
            grep_truncation(
                value.client_truncated,
                value.ripgrep_truncated,
                value.total_matched_lines,
                "matched lines",
                lines,
            );
        }
        None => {}
    }
}

fn grep_truncation(
    client_truncated: bool,
    ripgrep_truncated: bool,
    total: i32,
    unit: &str,
    lines: &mut Vec<String>,
) {
    if client_truncated || ripgrep_truncated {
        lines.push(format!("[Results truncated; at least {total} {unit}]"));
    }
}

fn diagnostics(value: &pb::DiagnosticsResult) -> Result<(String, bool)> {
    use pb::diagnostics_result::Result as R;
    match value
        .result
        .as_ref()
        .ok_or_else(|| missing("diagnostics"))?
    {
        R::Success(value) => Ok((diagnostics_success(value), false)),
        R::Error(value) => Ok((format!("{}: {}", value.path, value.error), true)),
        R::Rejected(value) => Ok((format!("{}: {}", value.path, value.reason), true)),
        R::FileNotFound(value) => Ok((format!("file not found: {}", value.path), true)),
        R::PermissionDenied(value) => Ok((format!("permission denied: {}", value.path), true)),
    }
}

fn diagnostics_success(value: &pb::DiagnosticsSuccess) -> String {
    if value.diagnostics.is_empty() {
        return format!("No diagnostics found in {}", value.path);
    }
    let mut lines = value
        .diagnostics
        .iter()
        .map(|diagnostic| {
            let location = diagnostic_location(&value.path, diagnostic.range.as_ref());
            let mut labels = vec![diagnostic_severity(diagnostic.severity)];
            if !diagnostic.source.is_empty() {
                labels.push(diagnostic.source.as_str());
            }
            if !diagnostic.code.is_empty() {
                labels.push(diagnostic.code.as_str());
            }
            if diagnostic.is_stale {
                labels.push("stale");
            }
            format!(
                "{}: [{}] {}",
                location,
                labels.join(" "),
                diagnostic.message
            )
        })
        .collect::<Vec<_>>();
    if value.total_diagnostics != value.diagnostics.len() as i32 {
        lines.push(format!(
            "[Reported {} diagnostics; received {} details]",
            value.total_diagnostics,
            value.diagnostics.len()
        ));
    }
    lines.join("\n")
}

fn diagnostic_location(path: &str, range: Option<&pb::Range>) -> String {
    let Some(range) = range else {
        return path.into();
    };
    let Some(start) = &range.start else {
        return path.into();
    };
    let mut location = format!(
        "{}:{}:{}",
        path,
        start.line.saturating_add(1),
        start.column.saturating_add(1)
    );
    if let Some(end) = &range.end {
        location.push_str(&format!(
            "-{}:{}",
            end.line.saturating_add(1),
            end.column.saturating_add(1)
        ));
    }
    location
}

fn diagnostic_severity(value: i32) -> &'static str {
    match pb::DiagnosticSeverity::try_from(value) {
        Ok(pb::DiagnosticSeverity::Error) => "error",
        Ok(pb::DiagnosticSeverity::Warning) => "warning",
        Ok(pb::DiagnosticSeverity::Information) => "information",
        Ok(pb::DiagnosticSeverity::Hint) => "hint",
        Ok(pb::DiagnosticSeverity::Unspecified) | Err(_) => "diagnostic",
    }
}

fn mcp(value: &pb::McpResult) -> Result<(String, bool)> {
    use pb::mcp_result::Result as R;
    match value.result.as_ref().ok_or_else(|| missing("mcp"))? {
        R::Success(value) => Ok((mcp_content(value)?, value.is_error)),
        R::Error(value) => Ok((value.error.clone(), true)),
        R::Rejected(value) => Ok((value.reason.clone(), true)),
        R::PermissionDenied(value) => Ok((value.error.clone(), true)),
        R::ToolNotFound(value) => Ok((format!("MCP tool not found: {}", value.name), true)),
        R::ServerNotFound(value) => Ok((format!("MCP server not found: {}", value.name), true)),
        R::Approved(_) => Err(Error::Protocol("MCP approval is not terminal".into())),
    }
}

fn mcp_content(success: &pb::McpSuccess) -> Result<String> {
    let mut content = Vec::new();
    for item in &success.content {
        match item.content.as_ref() {
            Some(pb::mcp_tool_result_content_item::Content::Text(text)) => {
                if !text.text.is_empty() {
                    content.push(text.text.clone());
                }
                if let Some(location) = &text.output_location {
                    content.push(format!(
                        "MCP output file: {} ({} bytes, {} lines)",
                        location.file_path, location.size_bytes, location.line_count
                    ));
                }
            }
            Some(pb::mcp_tool_result_content_item::Content::Image(image)) => content.push(format!(
                "MCP image: {} ({} bytes)",
                image.mime_type,
                image.data.len()
            )),
            None => {}
        }
    }
    if let Some(structured) = &success.structured_content {
        let value = serde_json::Value::Object(
            structured
                .fields
                .iter()
                .map(|(key, value)| (key.clone(), super::super::prost_json(value)))
                .collect(),
        );
        content.push(serde_json::to_string_pretty(&value)?);
    }
    Ok(if content.is_empty() {
        "MCP tool completed without content".into()
    } else {
        content.join("\n\n")
    })
}

fn read_mcp(value: &pb::ReadMcpResourceExecResult) -> Result<(String, bool)> {
    use pb::read_mcp_resource_exec_result::Result as R;
    match value
        .result
        .as_ref()
        .ok_or_else(|| missing("read MCP resource"))?
    {
        R::Success(value) => Ok((
            match value.content.as_ref() {
                Some(pb::read_mcp_resource_success::Content::Text(text)) => text.clone(),
                Some(pb::read_mcp_resource_success::Content::Blob(blob)) => {
                    format!("read MCP resource blob={}", blob.len())
                }
                None => format!("read MCP resource uri={}", value.uri),
            },
            false,
        )),
        R::Error(value) => Ok((value.error.clone(), true)),
        R::Rejected(value) => Ok((value.reason.clone(), true)),
        R::NotFound(value) => Ok((format!("MCP resource not found: {}", value.uri), true)),
    }
}

fn task(value: &pb::SubagentResult, call: &ToolCall) -> Result<(String, bool)> {
    use pb::subagent_result::Result as R;
    match value.result.as_ref().ok_or_else(|| missing("subagent"))? {
        R::Success(value) => {
            if value.agent_id.is_empty() {
                return Err(Error::Protocol("Task result is missing agent_id".into()));
            }
            let identity = call
                .arguments
                .get("description")
                .and_then(serde_json::Value::as_str)
                .filter(|name| !name.is_empty())
                .map_or_else(
                    || format!("Subagent ID: {}", value.agent_id),
                    |name| format!("Subagent name: {name}\nSubagent ID: {}", value.agent_id),
                );
            let content = value
                .final_message
                .as_deref()
                .filter(|message| !message.is_empty())
                .map_or(identity.clone(), |message| {
                    format!("{identity}\n\n{message}")
                });
            Ok((content, false))
        }
        R::Error(value) => Ok((value.error.clone(), true)),
    }
}

fn missing(name: &str) -> Error {
    Error::Protocol(format!("{name} returned no result"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn read_lints(paths: serde_json::Value) -> ToolCall {
        ToolCall {
            index: 0,
            call_id: "lints".into(),
            model_call_id: "model:0".into(),
            name: "ReadLints".into(),
            arguments_text: String::new(),
            arguments: json!({ "paths": paths }),
            argument_error: None,
        }
    }

    fn clean_result(path: &str) -> pb::exec_client_message::Message {
        pb::exec_client_message::Message::DiagnosticsResult(pb::DiagnosticsResult {
            result: Some(pb::diagnostics_result::Result::Success(
                pb::DiagnosticsSuccess {
                    path: path.into(),
                    diagnostics: Vec::new(),
                    total_diagnostics: 0,
                },
            )),
        })
    }

    /// 逐路径渲染在聚合前完成:单路径逐字不变,多路径只渲染本次结果。
    #[test]
    fn read_lints_renders_each_path_result_before_aggregation() {
        let single = read_lints(json!(["a.ts"]));
        let (content, is_error) = output(&clean_result("a.ts"), &single).unwrap();
        assert!(!is_error);
        assert_eq!(content, "No diagnostics found in a.ts");

        let many = read_lints(json!(["a.ts", "b.ts", "c.ts"]));
        let (content, is_error) = output(&clean_result("a.ts"), &many).unwrap();
        assert!(!is_error);
        assert_eq!(content, "No diagnostics found in a.ts");
    }

    #[test]
    fn read_lints_reports_this_paths_diagnostics() {
        let call = read_lints(json!(["a.ts", "b.ts"]));
        let message = pb::exec_client_message::Message::DiagnosticsResult(pb::DiagnosticsResult {
            result: Some(pb::diagnostics_result::Result::Success(
                pb::DiagnosticsSuccess {
                    path: "a.ts".into(),
                    diagnostics: vec![pb::Diagnostic {
                        message: "unused import".into(),
                        severity: pb::DiagnosticSeverity::Warning as i32,
                        ..Default::default()
                    }],
                    total_diagnostics: 1,
                },
            )),
        });
        let (content, _) = output(&message, &call).unwrap();
        assert!(content.contains("unused import"));
        assert!(!content.contains("Not checked"));
    }
}
