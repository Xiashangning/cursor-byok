use serde_json::json;

use crate::{
    cursor::{protocol::proto::agent::v1 as pb, tools::codec},
    model::{ToolCall, ToolResult},
    Error, Result,
};

use super::ToolCompletion;
use crate::cursor::tools::runtime::{now_ms, PendingShellAwait};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ShellCompletion {
    pub shell_id: String,
    pub status: pb::BackgroundTaskStatus,
    pub detail: Option<String>,
    pub output_path: Option<String>,
    pub runtime_ms: Option<u64>,
    pub output_length: Option<u64>,
    pub exit_code: Option<i32>,
}

impl ShellCompletion {
    pub(crate) fn from_notification(completion: &pb::BackgroundTaskCompletion) -> Result<Self> {
        let status = task_status(completion.status)?;
        Ok(Self {
            shell_id: completion.task_id.clone(),
            status,
            detail: completion.detail.clone(),
            output_path: completion.output_path.clone(),
            runtime_ms: None,
            output_length: None,
            exit_code: None,
        })
    }
}

pub(crate) fn completed(
    pending: PendingShellAwait,
    completion: ShellCompletion,
) -> Result<ToolCompletion> {
    let status = task_status_name(completion.status);
    let runtime_ms = completion
        .runtime_ms
        .unwrap_or_else(|| now_ms().saturating_sub(pending.started_at_ms));
    let output_length = completion.output_length.unwrap_or_default();
    let exit_code = completion
        .exit_code
        .or_else(|| completion.detail.as_deref().and_then(parse_exit_code))
        .or_else(|| (completion.status == pb::BackgroundTaskStatus::Success).then_some(0));
    let output_path = completion.output_path.clone().unwrap_or_default();
    let result = pb::AwaitTaskComplete {
        task_id: completion.shell_id.clone(),
        runtime_ms,
        output_file_path: output_path.clone(),
        output_length,
        regex_requested: false,
        regex_match: None,
        exit_code,
        wake_reason: completion.detail.clone(),
    };
    let mut content = match exit_code {
        Some(exit_code) => {
            format!("Task completed in {runtime_ms}ms with exit code: {exit_code}.")
        }
        None => format!("Task completed in {runtime_ms}ms with status: {status}."),
    };
    if !output_path.is_empty() {
        content.push_str(&format!(
            "\noutput_file_path: {output_path}\noutput_length: {output_length}"
        ));
    }
    finish(
        &pending.call,
        pending.started_at_ms,
        content,
        pb::await_success::AwaitResult::Complete(result),
    )
}

pub(crate) fn timed_out(pending: PendingShellAwait) -> Result<ToolCompletion> {
    let result = pb::AwaitTaskStillRunning {
        task_id: pending.shell_id.clone(),
        runtime_ms: now_ms().saturating_sub(pending.started_at_ms),
        output_file_path: String::new(),
        output_length: 0,
        regex_requested: false,
        regex_match: None,
        wake_reason: Some("timeout".into()),
    };
    finish(
        &pending.call,
        pending.started_at_ms,
        json!({
            "task_id": pending.shell_id,
            "status": "still_running",
            "wake_reason": "timeout",
        })
        .to_string(),
        pb::await_success::AwaitResult::StillRunning(result),
    )
}

fn finish(
    call: &ToolCall,
    started_at_ms: u64,
    content: String,
    result: pb::await_success::AwaitResult,
) -> Result<ToolCompletion> {
    let mut rendered = codec::render_tool_call(call, false)?;
    let Some(pb::tool_call::Tool::AwaitToolCall(mut tool)) = rendered.tool.take() else {
        return Err(Error::Protocol("Await has no Await representation".into()));
    };
    tool.result = Some(pb::AwaitResult {
        result: Some(pb::await_result::Result::Success(pb::AwaitSuccess {
            await_result: Some(result),
        })),
    });
    Ok(ToolCompletion::new(
        call,
        started_at_ms,
        ToolResult {
            call_id: call.call_id.clone(),
            content,
            is_error: false,
            image: None,
        },
        pb::tool_call::Tool::AwaitToolCall(tool),
    ))
}

fn parse_exit_code(detail: &str) -> Option<i32> {
    detail
        .split_whitespace()
        .collect::<Vec<_>>()
        .windows(2)
        .find_map(|fields| (fields[0] == "exit_code:").then_some(fields[1]))
        .or_else(|| {
            detail
                .split_whitespace()
                .find_map(|field| field.strip_prefix("exit_code:"))
        })
        .and_then(|value| value.parse().ok())
}

/// 后台任务状态的 proto→文本词汇。通知的写入侧(compile)与读取侧共用,
/// 任何一侧改动都必须同时生效。
pub(crate) fn task_status(value: i32) -> Result<pb::BackgroundTaskStatus> {
    let status = pb::BackgroundTaskStatus::try_from(value)
        .map_err(|_| Error::Protocol(format!("unknown background task status: {value}")))?;
    if status == pb::BackgroundTaskStatus::Unspecified {
        return Err(Error::Protocol(
            "background task completion has unspecified status".into(),
        ));
    }
    Ok(status)
}

pub(crate) fn task_status_name(status: pb::BackgroundTaskStatus) -> &'static str {
    match status {
        pb::BackgroundTaskStatus::Success => "success",
        pb::BackgroundTaskStatus::Error => "error",
        pb::BackgroundTaskStatus::Aborted => "aborted",
        pb::BackgroundTaskStatus::Unspecified => "unspecified",
    }
}
