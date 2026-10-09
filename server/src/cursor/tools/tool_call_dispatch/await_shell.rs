//! Polls Cursor terminal files while waiting for background shell tasks.

use std::{collections::HashSet, path::Path, time::Duration};

use crate::{
    cursor::protocol::proto::agent::v1 as pb,
    model::{CanonicalMessage, ContentPart, MessageContent, ToolCall},
    Error, Result,
};

use super::ToolStart;
use crate::cursor::tools::{
    codec,
    runtime::{
        await_arguments, now_ms, CursorToolRuntime, ExecContext, PendingExec, PendingShellAwait,
    },
    tool_call_result::{
        shell_await_completed, shell_await_timed_out, ShellCompletion, ToolCompletion,
        ToolResultSender,
    },
};

const BACKGROUND_SHELL_EVENT_PREFIX: &str = "background-completed:BACKGROUND_TASK_KIND_SHELL:";
const POLL_INTERVAL: Duration = Duration::from_millis(700);

pub(crate) struct MatchedShellAwaits {
    pub completions: Vec<ToolCompletion>,
    pub consumed: Vec<(String, String)>,
}

pub(crate) enum ShellAwaitPoll {
    Completed(Box<ToolCompletion>),
    Retry {
        delay: Duration,
        exec_id: u32,
        message: Box<pb::AgentServerMessage>,
    },
    Pending,
}

pub(crate) async fn complete_shell_awaits(
    runtime: &CursorToolRuntime,
    action: &pb::BackgroundTaskCompletionAction,
) -> Result<Option<MatchedShellAwaits>> {
    let mut notifications = Vec::new();
    let mut shell_ids = Vec::new();
    let mut unique = HashSet::new();
    for completion in &action.completions {
        let reason =
            pb::BackgroundTaskCompletionReason::try_from(completion.reason).map_err(|_| {
                Error::Protocol(format!(
                    "unknown background task completion reason: {}",
                    completion.reason
                ))
            })?;
        if reason != pb::BackgroundTaskCompletionReason::TaskFinished {
            continue;
        }
        let kind = pb::BackgroundTaskKind::try_from(completion.kind).map_err(|_| {
            Error::Protocol(format!("unknown background task kind: {}", completion.kind))
        })?;
        if kind != pb::BackgroundTaskKind::Shell {
            return Ok(None);
        }
        if completion.task_id.is_empty() || !unique.insert(completion.task_id.clone()) {
            return Err(Error::Protocol(
                "background shell completion has an invalid or duplicate task_id".into(),
            ));
        }
        let tool_call_id = completion
            .tool_call_id
            .as_deref()
            .filter(|value| !value.is_empty())
            .ok_or_else(|| {
                Error::Protocol("background shell completion has no tool_call_id".into())
            })?;
        shell_ids.push(completion.task_id.clone());
        notifications.push((
            ShellCompletion::from_notification(completion)?,
            tool_call_id.to_string(),
        ));
    }
    if notifications.is_empty() {
        return Ok(None);
    }
    let Some(pending) = runtime.take_shell_awaits(&shell_ids).await else {
        return Ok(None);
    };
    let mut completions = Vec::with_capacity(notifications.len());
    let mut consumed = Vec::with_capacity(notifications.len());
    for ((notification, tool_call_id), await_state) in notifications.into_iter().zip(pending) {
        consumed.push((notification.shell_id.clone(), tool_call_id));
        completions.push(shell_await_completed(await_state, notification)?);
    }
    Ok(Some(MatchedShellAwaits {
        completions,
        consumed,
    }))
}

pub(super) async fn start(
    runtime: &CursorToolRuntime,
    results: &ToolResultSender,
    call: &ToolCall,
    context: &ExecContext,
    messages: &[CanonicalMessage],
) -> Result<ToolStart> {
    let args = await_arguments(call)?;
    let shell_id = args.task_id.as_str();

    if let Some(completion) = completed_shell(messages, shell_id) {
        let started_at_ms = now_ms();
        let pending = PendingShellAwait {
            call: call.clone(),
            shell_id: shell_id.to_string(),
            shell_call_id: None,
            started_at_ms,
        };
        return Ok(ToolStart {
            messages: Vec::new(),
            completion: Some(shell_await_completed(pending, completion)?),
        });
    }

    let block_until_ms = u64::from(args.block_until_ms.expect("Await timeout is resolved"));
    if block_until_ms == 0 {
        let pending = PendingShellAwait {
            call: call.clone(),
            shell_id: shell_id.to_string(),
            shell_call_id: None,
            started_at_ms: now_ms(),
        };
        return Ok(ToolStart {
            messages: Vec::new(),
            completion: Some(shell_await_timed_out(pending)?),
        });
    }

    let path = terminal_path(&context.terminals_folder, shell_id)?;
    let pending = runtime
        .reserve_shell_await(
            call,
            shell_id.to_string(),
            backgrounded_shell_call(messages, shell_id),
        )
        .await?;
    let id = runtime
        .reserve_shell_poll(call, context, shell_id, pending.started_at_ms)
        .await?
        .expect("newly reserved shell Await must accept its first poll");
    let request = codec::shell_await_read_request(id, call, path);

    let runtime = runtime.clone();
    let results = results.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(block_until_ms)).await;
        let Some(pending) = runtime
            .take_shell_await(&pending.shell_id, &pending.call.call_id)
            .await
        else {
            return;
        };
        match shell_await_timed_out(pending) {
            Ok(completion) => results.send(completion),
            Err(error) => results.send_error(error),
        }
    });

    Ok(ToolStart {
        messages: vec![request],
        completion: None,
    })
}

pub(crate) async fn advance_poll(
    runtime: &CursorToolRuntime,
    pending: PendingExec,
    wire_result: &pb::exec_client_message::Message,
) -> Result<ShellAwaitPoll> {
    let args = await_arguments(&pending.call)?;
    let shell_id = args.task_id;
    let read = match wire_result {
        pb::exec_client_message::Message::ReadResult(result)
        | pb::exec_client_message::Message::RedactedReadResult(result) => result,
        _ => {
            return Err(Error::Protocol(
                "background shell Await expected ReadResult".into(),
            ))
        }
    };

    if let Some(completion) = completion_from_read(read, &shell_id) {
        return finish_poll(
            runtime,
            &shell_id,
            &pending.call.call_id,
            true,
            |await_state| shell_await_completed(await_state, completion),
        )
        .await;
    }

    let now = now_ms();
    let deadline_at_ms = pending.started_at_ms.saturating_add(u64::from(
        args.block_until_ms.expect("Await timeout is resolved"),
    ));
    if now >= deadline_at_ms {
        return finish_poll(
            runtime,
            &shell_id,
            &pending.call.call_id,
            false,
            shell_await_timed_out,
        )
        .await;
    }
    if deadline_at_ms.saturating_sub(now) <= POLL_INTERVAL.as_millis() as u64 {
        return Ok(ShellAwaitPoll::Pending);
    }

    let Some(id) = runtime
        .reserve_shell_poll(
            &pending.call,
            &pending.context,
            &shell_id,
            pending.started_at_ms,
        )
        .await?
    else {
        return Ok(ShellAwaitPoll::Pending);
    };
    let path = terminal_path(&pending.context.terminals_folder, &shell_id)?;
    Ok(ShellAwaitPoll::Retry {
        delay: POLL_INTERVAL,
        exec_id: id,
        message: Box::new(codec::shell_await_read_request(id, &pending.call, path)),
    })
}

/// Handles the race between a poll result and the notification path: only one
/// side owns the pending Await, and the loser returns `Pending` without a completion.
/// `record_consumed` 仅在轮询判定终态时为真:完成项携带台账身份
/// (shell_id, 原始 Shell 调用 id),由会话层落账,抑制随后的重复通知;
/// 超时不代表终态,不落账。
async fn finish_poll(
    runtime: &CursorToolRuntime,
    shell_id: &str,
    call_id: &str,
    record_consumed: bool,
    completion: impl FnOnce(PendingShellAwait) -> Result<ToolCompletion>,
) -> Result<ShellAwaitPoll> {
    match runtime.take_shell_await(shell_id, call_id).await {
        Some(await_state) => {
            let consumed = if record_consumed {
                await_state
                    .shell_call_id
                    .clone()
                    .map(|call_id| (shell_id.to_string(), call_id))
            } else {
                None
            };
            let mut completion = completion(await_state)?;
            completion.consumed_background = consumed;
            Ok(ShellAwaitPoll::Completed(Box::new(completion)))
        }
        None => Ok(ShellAwaitPoll::Pending),
    }
}

fn terminal_path(terminals_folder: &str, shell_id: &str) -> Result<String> {
    if terminals_folder.trim().is_empty() {
        return Err(Error::Protocol(
            "Await cannot poll a shell without the window terminals folder".into(),
        ));
    }
    Ok(Path::new(terminals_folder)
        .join(format!("{shell_id}.txt"))
        .to_string_lossy()
        .into_owned())
}

fn completion_from_read(result: &pb::ReadResult, shell_id: &str) -> Option<ShellCompletion> {
    let pb::read_result::Result::Success(success) = result.result.as_ref()? else {
        return None;
    };
    let content = match success.output.as_ref()? {
        pb::read_success::Output::Content(content) => content.as_str(),
        pb::read_success::Output::Data(data) => std::str::from_utf8(data).ok()?,
    };
    let status = match field(content, "status")? {
        "succeeded" => pb::BackgroundTaskStatus::Success,
        "failed" | "error" => pb::BackgroundTaskStatus::Error,
        "aborted" | "cancelled" => pb::BackgroundTaskStatus::Aborted,
        _ => return None,
    };
    let runtime_ms = field(content, "elapsed_ms")
        .or_else(|| field(content, "running_for_ms"))
        .and_then(|value| value.parse().ok());
    let exit_code = field(content, "exit_code").and_then(|value| value.parse().ok());
    Some(ShellCompletion {
        shell_id: shell_id.into(),
        status,
        detail: None,
        output_path: Some(success.path.clone()),
        runtime_ms,
        output_length: u64::try_from(success.file_size).ok(),
        exit_code,
    })
}

fn field<'a>(content: &'a str, name: &str) -> Option<&'a str> {
    content.lines().find_map(|line| {
        let (key, value) = line.split_once(':')?;
        (key.trim() == name).then(|| value.trim())
    })
}

fn completed_shell(messages: &[CanonicalMessage], shell_id: &str) -> Option<ShellCompletion> {
    let prefix = format!("{BACKGROUND_SHELL_EVENT_PREFIX}{shell_id}:");
    messages.iter().rev().find_map(|message| {
        message
            .runtime_event_id
            .as_deref()
            .is_some_and(|event_id| event_id.starts_with(&prefix))
            .then(|| parse_completion(message, shell_id))?
    })
}

/// 后台化该 shell 的原始 Shell 调用 id:按历史解析;模型可以凭空给出
/// shell_id,解析不到时返回 None(完成照常返回,只是不落账)。
/// 通知的 tool_call_id 即该调用的 id,台账身份必须与之相同。
fn backgrounded_shell_call(messages: &[CanonicalMessage], shell_id: &str) -> Option<String> {
    const BACKGROUNDED_PREFIX: &str = "shell running in background ";
    messages.iter().rev().find_map(|message| {
        let MessageContent::ToolResult(result) = &message.content else {
            return None;
        };
        let fields = result.content.strip_prefix(BACKGROUNDED_PREFIX)?;
        let id = fields
            .split(['\n', ' '])
            .find_map(|field| field.strip_prefix("shell_id="))?;
        (id == shell_id).then(|| result.call_id.clone())
    })
}

fn parse_completion(message: &CanonicalMessage, shell_id: &str) -> Option<ShellCompletion> {
    let text = match &message.content {
        MessageContent::Parts { parts } => parts.iter().find_map(|part| match part {
            ContentPart::Text { text } => Some(text.as_str()),
            _ => None,
        })?,
        _ => return None,
    };
    let status = match field(text, "status")? {
        "success" => pb::BackgroundTaskStatus::Success,
        "error" => pb::BackgroundTaskStatus::Error,
        "aborted" => pb::BackgroundTaskStatus::Aborted,
        _ => return None,
    };
    Some(ShellCompletion {
        shell_id: shell_id.into(),
        status,
        detail: field(text, "detail").map(str::to_owned),
        output_path: field(text, "output_path").map(str::to_owned),
        runtime_ms: None,
        output_length: None,
        exit_code: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Origin, Role, ToolResultContent};

    fn await_call(block_until_ms: u32) -> ToolCall {
        ToolCall {
            index: 0,
            call_id: "await-call".into(),
            model_call_id: "model-call".into(),
            name: "Await".into(),
            arguments_text: format!("{{\"shell_id\":\"42\",\"block_until_ms\":{block_until_ms}}}"),
            arguments: serde_json::json!({
                "shell_id": "42",
                "block_until_ms": block_until_ms,
            }),
            argument_error: None,
        }
    }

    #[tokio::test]
    async fn timeout_clears_the_in_flight_terminal_read() {
        let runtime = CursorToolRuntime::default();
        let (results, mut receiver) = crate::cursor::tools::tool_call_result::tool_result_channel();
        let call = await_call(10);
        let context = ExecContext {
            terminals_folder: "/remote/.cursor/terminals".into(),
            ..ExecContext::default()
        };

        let started = start(&runtime, &results, &call, &context, &[])
            .await
            .unwrap();
        assert_eq!(started.messages.len(), 1);
        assert_eq!(runtime.running_exec_ids().await.len(), 1);

        let completion = tokio::time::timeout(Duration::from_secs(1), receiver.recv())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(matches!(
            &completion.tool_call().tool,
            Some(pb::tool_call::Tool::AwaitToolCall(pb::AwaitToolCall {
                result: Some(pb::AwaitResult {
                    result: Some(pb::await_result::Result::Success(pb::AwaitSuccess {
                        await_result: Some(pb::await_success::AwaitResult::StillRunning(_)),
                    })),
                }),
                ..
            }))
        ));
        assert!(runtime.running_exec_ids().await.is_empty());
    }

    #[test]
    fn finds_a_previously_projected_shell_completion() {
        let shell_id = "42";
        let mut message = CanonicalMessage::text(
            "runtime:test",
            Role::User,
            Origin::Runtime,
            "<system_notification>\nstatus: success\ntask_id: 42\ndetail: finished\noutput_path: /tmp/42.txt\n</system_notification>",
        );
        message.runtime_event_id = Some(format!(
            "{BACKGROUND_SHELL_EVENT_PREFIX}{shell_id}:shell-call"
        ));

        let completion = completed_shell(&[message], shell_id).unwrap();
        assert_eq!(completion.shell_id, shell_id);
        assert_eq!(completion.status, pb::BackgroundTaskStatus::Success);
        assert_eq!(completion.detail.as_deref(), Some("finished"));
        assert_eq!(completion.output_path.as_deref(), Some("/tmp/42.txt"));
    }

    fn backgrounded_shell_result(shell_id: &str, call_id: &str) -> CanonicalMessage {
        CanonicalMessage {
            message_id: format!("tool:{call_id}"),
            role: Role::Tool,
            origin: Origin::Tool,
            content: MessageContent::ToolResult(ToolResultContent {
                call_id: call_id.into(),
                name: "Shell".into(),
                content: format!(
                    "shell running in background shell_id={shell_id} pid=7 terminals_folder=/tmp/terminals\nCursor will notify you when it finishes."
                ),
                is_error: false,
                image: None,
                provider_parts: Vec::new(),
            }),
            runtime_event_id: None,
        }
    }

    fn terminal_read() -> pb::exec_client_message::Message {
        pb::exec_client_message::Message::ReadResult(pb::ReadResult {
            result: Some(pb::read_result::Result::Success(pb::ReadSuccess {
                path: "/tmp/terminals/42.txt".into(),
                file_size: 10,
                output: Some(pb::read_success::Output::Content(
                    "status: succeeded\nelapsed_ms: 5\nexit_code: 0\n".into(),
                )),
                ..Default::default()
            })),
        })
    }

    /// 轮询先于通知判定终态:完成项携带 (shell_id, 原始 Shell 调用 id) 台账身份,
    /// 由会话层落账,使随后到达的同一完成通知被抑制。
    #[tokio::test]
    async fn a_poll_detected_terminal_state_carries_the_ledger_identity() {
        let runtime = CursorToolRuntime::default();
        let (results, _receiver) = crate::cursor::tools::tool_call_result::tool_result_channel();
        let call = await_call(50_000);
        let context = ExecContext {
            terminals_folder: "/tmp/terminals".into(),
            ..ExecContext::default()
        };
        let history = [
            backgrounded_shell_result("421", "other-shell-call"),
            backgrounded_shell_result("42", "shell-call"),
        ];
        start(&runtime, &results, &call, &context, &history)
            .await
            .unwrap();
        let id = runtime.running_exec_ids().await[0];
        let pending = runtime.take_exec(id).await.unwrap();

        let poll = advance_poll(&runtime, pending, &terminal_read())
            .await
            .unwrap();

        let ShellAwaitPoll::Completed(completion) = poll else {
            panic!("terminal read must complete the Await")
        };
        assert_eq!(
            completion.consumed_background,
            Some(("42".to_owned(), "shell-call".to_owned())),
            "the identity must match the notification's tool_call_id exactly"
        );
    }

    /// 历史里没有对应的后台化结果时无法构造台账身份:完成照常返回,
    /// 由通知路径自行投影(退化为现状,不误抑制)。
    #[tokio::test]
    async fn a_poll_terminal_state_without_a_known_shell_call_records_nothing() {
        let runtime = CursorToolRuntime::default();
        let (results, _receiver) = crate::cursor::tools::tool_call_result::tool_result_channel();
        let call = await_call(50_000);
        let context = ExecContext {
            terminals_folder: "/tmp/terminals".into(),
            ..ExecContext::default()
        };
        start(&runtime, &results, &call, &context, &[])
            .await
            .unwrap();
        let id = runtime.running_exec_ids().await[0];
        let pending = runtime.take_exec(id).await.unwrap();

        let poll = advance_poll(&runtime, pending, &terminal_read())
            .await
            .unwrap();

        let ShellAwaitPoll::Completed(completion) = poll else {
            panic!("terminal read must complete the Await")
        };
        assert_eq!(completion.consumed_background, None);
    }
}
