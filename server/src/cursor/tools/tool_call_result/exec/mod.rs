//! Coordinates command execution Tool results.
mod output;
mod render;

use crate::{
    cursor::{protocol::proto::agent::v1 as pb, tools::codec as interaction},
    model::ToolResult,
    Error, Result,
};

use super::{gate, mcp_state, ReadImage, ToolCompletion};
use crate::cursor::tools::{
    edit,
    runtime::{ExecStage, PendingExec},
};

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
    if let Message::ForceBackgroundSubagentResult(result) = wire_result {
        return background_subagent(pending, result);
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
            consumed_completion: None,
        },
        tool,
    )
    .with_read_image(read_image))
}

fn background_subagent(
    pending: PendingExec,
    result: &pb::ForceBackgroundSubagentResult,
) -> Result<ToolCompletion> {
    let mut rendered = interaction::render_tool_call(&pending.call, false)?;
    let Some(pb::tool_call::Tool::TaskToolCall(mut tool)) = rendered.tool.take() else {
        return Err(Error::Protocol(
            "create-agent has no Task representation".into(),
        ));
    };
    let accepted = result.status == pb::ForceBackgroundSubagentStatus::Accepted as i32;
    let message = if accepted {
        "background agent accepted"
    } else {
        "background agent was not found"
    };
    tool.result = Some(pb::TaskResult {
        result: Some(if accepted {
            pb::task_result::Result::Success(pb::TaskSuccess {
                is_background: true,
                result_suffix: Some(message.into()),
                ..Default::default()
            })
        } else {
            pb::task_result::Result::Error(pb::TaskError {
                error: message.into(),
            })
        }),
    });
    Ok(orchestration_completion(
        &pending,
        message.into(),
        !accepted,
        pb::tool_call::Tool::TaskToolCall(tool),
    ))
}

fn subagent_await(
    pending: PendingExec,
    result: &pb::SubagentAwaitResult,
) -> Result<ToolCompletion> {
    let mut rendered = interaction::render_tool_call(&pending.call, false)?;
    let Some(pb::tool_call::Tool::AwaitToolCall(mut tool)) = rendered.tool.take() else {
        return Err(Error::Protocol("AWAIT has no Await representation".into()));
    };
    let (content, is_error, await_result, _consumed_task_id) = match result.result.as_ref() {
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
            Some(value.agent_id.clone()),
        ),
        Some(pb::subagent_await_result::Result::StillRunning(value)) => (
            "background agent is still running".into(),
            false,
            pb::await_result::Result::StillRunning(pb::AwaitTaskStillRunning {
                task_id: value.agent_id.clone(),
                output_file_path: value.transcript_path.clone().unwrap_or_default(),
                ..Default::default()
            }),
            None,
        ),
        Some(pb::subagent_await_result::Result::NotFound(value)) => (
            format!("background agent not found: {}", value.agent_id),
            true,
            pb::await_result::Result::Error(pb::AwaitError {
                error: format!("background agent not found: {}", value.agent_id),
            }),
            None,
        ),
        Some(pb::subagent_await_result::Result::Error(value)) => (
            value.error.clone(),
            true,
            pb::await_result::Result::Error(pb::AwaitError {
                error: value.error.clone(),
            }),
            None,
        ),
        None => return Err(Error::Protocol("AWAIT returned no result".into())),
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
            consumed_completion: None,
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
    Some(ReadImage {
        mime_type: image_mime_type(data)?.into(),
        data: data.clone(),
        path: success.path.clone(),
    })
}

fn image_mime_type(data: &[u8]) -> Option<&'static str> {
    let reader = image::ImageReader::new(std::io::Cursor::new(data))
        .with_guessed_format()
        .ok()?;
    let format = reader.format()?;
    let (width, height) = reader.into_dimensions().ok()?;
    if width == 0 || height == 0 {
        return None;
    }
    match format {
        image::ImageFormat::Png => Some("image/png"),
        image::ImageFormat::Jpeg => Some("image/jpeg"),
        image::ImageFormat::Gif => Some("image/gif"),
        image::ImageFormat::WebP => Some("image/webp"),
        _ => None,
    }
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
            consumed_completion: None,
        },
        pb::tool_call::Tool::EditToolCall(tool),
    ))
}
#[cfg(test)]
mod tests {
    use base64::{engine::general_purpose::STANDARD, Engine};
    use serde_json::json;

    use super::{from_exec, image_mime_type};
    use crate::cursor::protocol::proto::agent::v1 as pb;
    use crate::cursor::tools::runtime::{ExecContext, ExecStage, PendingExec};
    use crate::model::ToolCall;

    #[test]
    fn read_image_requires_a_decodable_supported_image() {
        let png = STANDARD
            .decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=")
            .unwrap();
        assert_eq!(image_mime_type(&png), Some("image/png"));
        assert_eq!(image_mime_type(b"\x89PNG\r\n\x1a\n"), None);
    }

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
    fn orchestration_results_become_cursor_tool_completions() {
        let completion = from_exec(
            pending(
                "create-agent",
                json!({"title":"Inspect","prompt":"inspect"}),
            ),
            &pb::exec_client_message::Message::ForceBackgroundSubagentResult(
                pb::ForceBackgroundSubagentResult {
                    status: pb::ForceBackgroundSubagentStatus::Accepted as i32,
                },
            ),
        )
        .unwrap();
        assert!(!completion.result().is_error);
        assert!(matches!(
            completion.tool_call().tool,
            Some(pb::tool_call::Tool::TaskToolCall(_))
        ));

        let completion = from_exec(
            pending("AWAIT", json!({"task_id":"agent-1"})),
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
