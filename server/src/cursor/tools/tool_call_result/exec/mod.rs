//! Coordinates command execution Tool results.
mod output;
mod render;

use crate::{
    cursor::{protocol::proto::agent::v1 as pb, tools::codec as interaction},
    model::{image_metadata, ToolResult},
    Error, Result,
};

use super::{gate, mcp_state, ReadImage, ToolCompletion};
use crate::cursor::tools::{
    edit,
    runtime::{ExecStage, PendingExec},
};

pub(crate) fn complete_diagnostics(
    pending: PendingExec,
    results: &[pb::DiagnosticsResult],
) -> Result<ToolCompletion> {
    let mut text = Vec::new();
    let mut files = Vec::new();
    let mut total_diagnostics = 0i32;
    let mut failed = false;
    for result in results {
        let (content, is_error) = output::output(
            &pb::exec_client_message::Message::DiagnosticsResult(result.clone()),
            &pending.call,
        )?;
        text.push(content);
        failed |= is_error;
        if let Some(pb::read_lints_tool_result::Result::Success(success)) =
            render::diagnostics(result)?.result
        {
            files.extend(success.file_diagnostics);
            total_diagnostics = total_diagnostics.saturating_add(success.total_diagnostics);
        }
    }
    let content = text.join("\n\n");
    let mut rendered = interaction::render_tool_call(&pending.call, false)?;
    let Some(pb::tool_call::Tool::ReadLintsToolCall(tool)) = rendered.tool.as_mut() else {
        unreachable!()
    };
    tool.result = Some(pb::ReadLintsToolResult {
        result: Some(if failed {
            pb::read_lints_tool_result::Result::Error(pb::ReadLintsToolError {
                error_message: content.clone(),
            })
        } else {
            pb::read_lints_tool_result::Result::Success(pb::ReadLintsToolSuccess {
                total_files: i32::try_from(files.len())
                    .map_err(|_| Error::Protocol("too many diagnostics files".into()))?,
                file_diagnostics: files,
                total_diagnostics,
            })
        }),
    });
    ToolCompletion::from_rendered(
        &pending.call,
        pending.started_at_ms,
        content,
        failed,
        rendered,
    )
}

pub(crate) fn from_exec(
    pending: PendingExec,
    wire_result: &pb::exec_client_message::Message,
) -> Result<ToolCompletion> {
    use pb::{exec_client_message::Message, tool_call::Tool};
    let mut gated_shell = matches!(
        wire_result,
        Message::ShellResult(_) | Message::MiniSweAgentBashResult(_)
    )
    .then(|| wire_result.clone());
    if let Some(message) = gated_shell.as_mut() {
        gate::exec_message(message);
    }
    let wire_result = gated_shell.as_ref().unwrap_or(wire_result);
    if let Message::McpStateExecResult(result) = wire_result {
        return mcp_state::complete(pending, result);
    }
    if let Message::SubagentAwaitResult(result) = wire_result {
        return subagent_await(pending, result);
    }
    let call = &pending.call;
    let read_image = read_image(wire_result);
    let (mut content, is_error) = output::output(wire_result, call)?;
    if let Some(image) = &read_image {
        content = format!("Read image file: {}", image.path);
    }
    let mut rendered = match &pending.stage {
        ExecStage::DynamicMcp(definition) => {
            interaction::render_dynamic_mcp(call, definition, false)
        }
        _ => interaction::render_tool_call(call, false)?,
    };
    match (rendered.tool.as_mut(), wire_result) {
        (Some(Tool::ShellToolCall(tool)), Message::ShellResult(result))
        | (Some(Tool::ShellToolCall(tool)), Message::MiniSweAgentBashResult(result)) => {
            tool.result = Some(result.clone());
        }
        (Some(Tool::DeleteToolCall(tool)), Message::DeleteResult(result)) => {
            tool.result = Some(result.clone());
        }
        (Some(Tool::GrepToolCall(tool)), Message::GrepResult(result)) => {
            tool.result = Some(result.clone());
        }
        (Some(Tool::GlobToolCall(tool)), Message::GrepResult(result)) => {
            tool.result = Some(render::glob(result)?);
        }
        (Some(Tool::ReadToolCall(tool)), Message::ReadResult(result))
        | (Some(Tool::ReadToolCall(tool)), Message::RedactedReadResult(result)) => {
            tool.result = Some(render::read(result, call)?);
        }
        (Some(Tool::ReadLintsToolCall(tool)), Message::DiagnosticsResult(result)) => {
            tool.result = Some(render::diagnostics(result)?);
        }
        (Some(Tool::McpToolCall(tool)), Message::McpResult(result)) => {
            tool.result = Some(render::mcp(result)?);
        }
        (Some(Tool::ReadMcpResourceToolCall(tool)), Message::ReadMcpResourceExecResult(result)) => {
            tool.result = Some(result.clone());
        }
        (Some(Tool::TaskToolCall(tool)), Message::SubagentResult(result)) => {
            tool.result = Some(render::task(result, call, pending.started_at_ms)?);
        }
        (Some(Tool::EditToolCall(tool)), Message::WriteResult(result)) => {
            tool.result = Some(match (&pending.stage, result.result.as_ref()) {
                (ExecStage::EditWrite(write), Some(pb::write_result::Result::Success(success))) => {
                    edit::success(success.path.clone(), write)
                }
                _ => render::write(result)?,
            });
        }
        _ => {
            return Err(Error::Protocol(format!(
                "unexpected Exec result for tool {}",
                call.name
            )));
        }
    }
    let tool = rendered.tool.ok_or_else(|| {
        Error::Protocol(format!("tool {} has no Cursor representation", call.name))
    })?;
    Ok(ToolCompletion::new(
        call,
        pending.started_at_ms,
        ToolResult {
            call_id: call.call_id.clone(),
            content,
            is_error,
            image: None,
        },
        tool,
    )
    .with_read_image(read_image))
}

fn subagent_await(
    pending: PendingExec,
    result: &pb::SubagentAwaitResult,
) -> Result<ToolCompletion> {
    let mut rendered = interaction::render_tool_call(&pending.call, false)?;
    let Some(pb::tool_call::Tool::AwaitToolCall(mut tool)) = rendered.tool.take() else {
        return Err(Error::Protocol("Await has no Await representation".into()));
    };
    let (content, is_error, await_result) = match result.result.as_ref() {
        Some(pb::subagent_await_result::Result::Complete(value)) => (
            value
                .final_message
                .clone()
                .unwrap_or_else(|| "background agent completed".into()),
            false,
            pb::await_result::Result::Complete(pb::AwaitTaskComplete {
                task_id: value.agent_id.clone(),
                output_file_path: value.transcript_path.clone().unwrap_or_default(),
                ..Default::default()
            }),
        ),
        Some(pb::subagent_await_result::Result::StillRunning(value)) => (
            "background agent is still running".into(),
            false,
            pb::await_result::Result::StillRunning(pb::AwaitTaskStillRunning {
                task_id: value.agent_id.clone(),
                output_file_path: value.transcript_path.clone().unwrap_or_default(),
                ..Default::default()
            }),
        ),
        Some(pb::subagent_await_result::Result::NotFound(value)) => (
            format!("background agent not found: {}", value.agent_id),
            true,
            pb::await_result::Result::Error(pb::AwaitError {
                error: format!("background agent not found: {}", value.agent_id),
            }),
        ),
        Some(pb::subagent_await_result::Result::Error(value)) => (
            value.error.clone(),
            true,
            pb::await_result::Result::Error(pb::AwaitError {
                error: value.error.clone(),
            }),
        ),
        None => return Err(Error::Protocol("Await returned no result".into())),
    };
    tool.result = Some(pb::AwaitResult {
        result: Some(await_result),
    });
    Ok(orchestration_completion(
        &pending,
        content,
        is_error,
        pb::tool_call::Tool::AwaitToolCall(tool),
    ))
}

fn orchestration_completion(
    pending: &PendingExec,
    content: String,
    is_error: bool,
    tool: pb::tool_call::Tool,
) -> ToolCompletion {
    let call = &pending.call;
    ToolCompletion::new(
        call,
        pending.started_at_ms,
        ToolResult {
            call_id: call.call_id.clone(),
            content,
            is_error,
            image: None,
        },
        tool,
    )
}

fn read_image(message: &pb::exec_client_message::Message) -> Option<ReadImage> {
    use pb::{exec_client_message::Message, read_result::Result, read_success::Output};
    let result = match message {
        Message::ReadResult(result) | Message::RedactedReadResult(result) => result,
        _ => return None,
    };
    let Result::Success(success) = result.result.as_ref()? else {
        return None;
    };
    let Output::Data(data) = success.output.as_ref()? else {
        return None;
    };
    let metadata = image_metadata(data)?;
    Some(ReadImage {
        mime_type: metadata.mime_type.into(),
        data: data.clone(),
        path: success.path.clone(),
    })
}

pub(crate) fn edit_failure(pending: PendingExec, error: String) -> Result<ToolCompletion> {
    let call = &pending.call;
    let mut rendered = interaction::render_tool_call(call, false)?;
    let Some(pb::tool_call::Tool::EditToolCall(mut tool)) = rendered.tool.take() else {
        return Err(Error::Protocol(format!(
            "{} is not an edit tool",
            call.name
        )));
    };
    tool.result = Some(edit::failure(edit::path(call)?, error.clone()));
    Ok(ToolCompletion::new(
        call,
        pending.started_at_ms,
        ToolResult {
            call_id: call.call_id.clone(),
            content: error,
            is_error: true,
            image: None,
        },
        pb::tool_call::Tool::EditToolCall(tool),
    ))
}
#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::from_exec;
    use crate::cursor::protocol::proto::agent::v1 as pb;
    use crate::cursor::tools::runtime::{ExecContext, ExecStage, PendingExec};
    use crate::model::ToolCall;

    fn pending(name: &str, arguments: serde_json::Value) -> PendingExec {
        PendingExec {
            call: ToolCall {
                index: 0,
                call_id: "call-1".into(),
                model_call_id: "model-1".into(),
                name: name.into(),
                arguments_text: arguments.to_string(),
                arguments,
                argument_error: None,
            },
            context: ExecContext::default(),
            started_at_ms: 1,
            stdout: String::new(),
            stderr: String::new(),
            stage: ExecStage::Direct,
        }
    }

    #[test]
    fn await_result_becomes_a_cursor_tool_completion() {
        let completion = from_exec(
            pending("Await", json!({"task_id":"agent-1"})),
            &pb::exec_client_message::Message::SubagentAwaitResult(pb::SubagentAwaitResult {
                result: Some(pb::subagent_await_result::Result::StillRunning(
                    pb::SubagentAwaitStillRunning {
                        agent_id: "agent-1".into(),
                        transcript_path: None,
                    },
                )),
            }),
        )
        .unwrap();
        assert!(!completion.result().is_error);
        assert!(matches!(
            completion.tool_call().tool,
            Some(pb::tool_call::Tool::AwaitToolCall(_))
        ));
    }
}
