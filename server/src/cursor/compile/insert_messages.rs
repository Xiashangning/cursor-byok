//! Compiles non-interrupting runtime information into append-only Messages.
use std::collections::BTreeMap;

use crate::{cursor::protocol::proto::agent::v1 as pb, Error, Result};

pub(super) const FOLLOW_UP: &str = concat!(
    "Perform any necessary follow-up actions in response to the subagent completion above. ",
    "If no follow-up work is needed, no further action is required. ",
    "If you mention an agent or subagent in your response, link it with the `[Name](id)` ",
    "Don't use generic label such as `[agent]`, `[worker]`, or `[subagent]`. ",
    "For cloud subagents, when the agent has edited code, link to `[Review](bc-id#changes)`, ",
    "or, if you know the exact added and deleted line counts, `[Review +A −D](bc-id#changes)`, ",
    "replacing A and D with those counts. Never write A or D literally. ",
    "Use `[Try Live](bc-id#desktop)` only when the agent used computer use. ",
    "Don't repeat the same confirmation every time."
);

pub(super) const SHELL_FOLLOW_UP: &str = concat!(
    "Briefly inform the user about the task result and perform any follow-up actions (if needed). ",
    "If there's no follow-ups needed, don't explicitly say that."
);

#[derive(Debug)]
pub(super) struct Projection {
    pub context: String,
    pub turn_user: pb::UserMessage,
}

pub(super) fn project(
    action: &pb::BackgroundTaskCompletionAction,
    mode: i32,
    state: Option<&pb::ConversationStateStructure>,
) -> Result<Projection> {
    if action.completions.is_empty() {
        return Err(Error::Protocol(
            "background task completion action contains no completion".into(),
        ));
    }

    let mut completions = BTreeMap::new();
    let mut has_shell = false;
    let mut has_subagent = false;
    for completion in &action.completions {
        let kind = pb::BackgroundTaskKind::try_from(completion.kind).map_err(|_| {
            Error::Protocol(format!("unknown background task kind: {}", completion.kind))
        })?;
        if kind == pb::BackgroundTaskKind::Unspecified {
            return Err(Error::Protocol(format!(
                "background task completion has invalid kind: {}",
                kind.as_str_name()
            )));
        }
        let reason =
            pb::BackgroundTaskCompletionReason::try_from(completion.reason).map_err(|_| {
                Error::Protocol(format!(
                    "unknown background task completion reason: {}",
                    completion.reason
                ))
            })?;
        if reason != pb::BackgroundTaskCompletionReason::TaskFinished {
            // Progress and reparenting notifications are informational; the
            // client batches them together with the real finish notification.
            continue;
        }
        if completion_consumed(completion, state) {
            continue;
        }
        if completion.task_id.is_empty() || completion.title.is_empty() {
            return Err(Error::Protocol(
                "background task completion requires task_id and title".into(),
            ));
        }
        let agent_id = match kind {
            pb::BackgroundTaskKind::Shell => {
                has_shell = true;
                None
            }
            pb::BackgroundTaskKind::Subagent => {
                has_subagent = true;
                Some(
                    completion
                        .subagent_id
                        .as_deref()
                        .filter(|id| !id.is_empty())
                        .ok_or_else(|| {
                            Error::Protocol(
                                "background subagent completion has no subagent_id".into(),
                            )
                        })?,
                )
            }
            pb::BackgroundTaskKind::Unspecified => unreachable!(),
        };
        let tool_call_id = completion
            .tool_call_id
            .as_deref()
            .filter(|id| !id.is_empty())
            .ok_or_else(|| {
                Error::Protocol("background task completion has no tool_call_id".into())
            })?;
        let task_identity = agent_id.unwrap_or(&completion.task_id);
        let identity = format!("{}:{task_identity}:{tool_call_id}", kind.as_str_name());
        let context = completion_context(completion, kind, agent_id)?;
        if completions
            .insert(identity.clone(), (completion, context))
            .is_some()
        {
            return Err(Error::Protocol(format!(
                "duplicate background task completion: {identity}"
            )));
        }
    }

    let (first, _) = completions.values().next().ok_or_else(|| {
        Error::Protocol("background task notification contains no finished task".into())
    })?;
    let text = match (has_shell, has_subagent) {
        (true, false) => SHELL_FOLLOW_UP.into(),
        (false, true) => FOLLOW_UP.into(),
        (true, true) => format!("{SHELL_FOLLOW_UP}\n\n{FOLLOW_UP}"),
        (false, false) => unreachable!(),
    };
    Ok(Projection {
        context: completions
            .values()
            .map(|(_, context)| context.as_str())
            .collect::<Vec<_>>()
            .join("\n\n"),
        turn_user: pb::UserMessage {
            text,
            message_id: format!(
                "background-completed:{}",
                completions.keys().cloned().collect::<Vec<_>>().join(":")
            ),
            mode,
            is_simulated_msg: Some(true),
            simulated_msg_reason: Some(pb::SimulatedMsgReason::BackgroundTaskCompletion as i32),
            simulated_message_metadata: Some(pb::user_message::SimulatedMessageMetadata {
                title: Some(first.title.clone()),
                task_id: Some(first.task_id.clone()),
                ..Default::default()
            }),
            ..Default::default()
        },
    })
}

pub(super) fn fully_consumed(
    action: &pb::BackgroundTaskCompletionAction,
    state: Option<&pb::ConversationStateStructure>,
) -> bool {
    let finished = action.completions.iter().filter(|completion| {
        completion.reason == pb::BackgroundTaskCompletionReason::TaskFinished as i32
    });
    let mut count = 0;
    for completion in finished {
        count += 1;
        if !completion_consumed(completion, state) {
            return false;
        }
    }
    count > 0
}

fn completion_consumed(
    completion: &pb::BackgroundTaskCompletion,
    state: Option<&pb::ConversationStateStructure>,
) -> bool {
    if completion.kind != pb::BackgroundTaskKind::Subagent as i32 {
        return false;
    }
    let (Some(agent_id), Some(tool_call_id), Some(state)) = (
        completion
            .subagent_id
            .as_deref()
            .filter(|id| !id.is_empty()),
        completion
            .tool_call_id
            .as_deref()
            .filter(|id| !id.is_empty()),
        state,
    ) else {
        return false;
    };
    let Some(run) = state.subagent_runs_by_parent_tool_call_id.get(tool_call_id) else {
        return false;
    };
    if run.subagent_id.as_deref() != Some(agent_id)
        || run.completion_reason != Some(pb::BackgroundTaskCompletionReason::TaskFinished as i32)
    {
        return false;
    }
    matches!(
        pb::SubagentRunStatus::try_from(run.status),
        Ok(pb::SubagentRunStatus::Success
            | pb::SubagentRunStatus::Error
            | pb::SubagentRunStatus::Aborted)
    ) && matches!(
        pb::BackgroundTaskStatus::try_from(completion.status),
        Ok(pb::BackgroundTaskStatus::Success
            | pb::BackgroundTaskStatus::Error
            | pb::BackgroundTaskStatus::Aborted)
    )
}

fn status(completion: &pb::BackgroundTaskCompletion) -> Result<pb::BackgroundTaskStatus> {
    let status = pb::BackgroundTaskStatus::try_from(completion.status).map_err(|_| {
        Error::Protocol(format!(
            "unknown background task status: {}",
            completion.status
        ))
    })?;
    if status == pb::BackgroundTaskStatus::Unspecified {
        return Err(Error::Protocol(
            "background task completion has unspecified status".into(),
        ));
    }
    Ok(status)
}

fn completion_context(
    completion: &pb::BackgroundTaskCompletion,
    kind: pb::BackgroundTaskKind,
    agent_id: Option<&str>,
) -> Result<String> {
    let status = status(completion)?;
    let mut fields = vec![
        format!(
            "kind: {}",
            match kind {
                pb::BackgroundTaskKind::Shell => "shell",
                pb::BackgroundTaskKind::Subagent => "subagent",
                pb::BackgroundTaskKind::Unspecified => unreachable!(),
            }
        ),
        format!("status: {}", status_name(status)),
        format!("task_id: {}", completion.task_id),
        format!("title: {}", completion.title),
    ];
    optional_field(
        &mut fields,
        "tool_call_id",
        completion.tool_call_id.as_deref(),
    );
    optional_field(&mut fields, "agent_id", agent_id);
    optional_field(&mut fields, "detail", completion.detail.as_deref());
    optional_field(
        &mut fields,
        "output_path",
        completion.output_path.as_deref(),
    );
    optional_field(&mut fields, "thread_id", completion.thread_id.as_deref());
    Ok(format!(
        "<system_notification>\nThe following task has finished. If you were already aware, ignore this notification and do not restate prior responses.\n\n<task>\n{}\n</task>\n</system_notification>",
        fields.join("\n")
    ))
}

fn optional_field(fields: &mut Vec<String>, name: &str, value: Option<&str>) {
    if let Some(value) = value.filter(|value| !value.is_empty()) {
        fields.push(format!("{name}: {value}"));
    }
}

fn status_name(status: pb::BackgroundTaskStatus) -> &'static str {
    match status {
        pb::BackgroundTaskStatus::Success => "success",
        pb::BackgroundTaskStatus::Error => "error",
        pb::BackgroundTaskStatus::Aborted => "aborted",
        pb::BackgroundTaskStatus::Unspecified => unreachable!(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn completion(agent_id: &str, tool_call_id: &str) -> pb::BackgroundTaskCompletion {
        pb::BackgroundTaskCompletion {
            task_id: agent_id.into(),
            kind: pb::BackgroundTaskKind::Subagent as i32,
            status: pb::BackgroundTaskStatus::Success as i32,
            title: format!("Agent {agent_id}"),
            reason: pb::BackgroundTaskCompletionReason::TaskFinished as i32,
            subagent_id: Some(agent_id.into()),
            tool_call_id: Some(tool_call_id.into()),
            ..Default::default()
        }
    }

    fn state(agent_id: &str, tool_call_id: &str) -> pb::ConversationStateStructure {
        pb::ConversationStateStructure {
            subagent_runs_by_parent_tool_call_id: HashMap::from([(
                tool_call_id.into(),
                pb::SubagentRunState {
                    parent_tool_call_id: tool_call_id.into(),
                    subagent_id: Some(agent_id.into()),
                    status: pb::SubagentRunStatus::Success as i32,
                    completion_reason: Some(
                        pb::BackgroundTaskCompletionReason::TaskFinished as i32,
                    ),
                    ..Default::default()
                },
            )]),
            ..Default::default()
        }
    }

    #[test]
    fn terminal_result_consumed_by_await_suppresses_the_follow_up_action() {
        let action = pb::BackgroundTaskCompletionAction {
            completions: vec![completion("agent-1", "task-call-1")],
        };
        let state = state("agent-1", "task-call-1");

        assert!(fully_consumed(&action, Some(&state)));
    }

    #[test]
    fn mixed_batch_keeps_only_unconsumed_completions() {
        let action = pb::BackgroundTaskCompletionAction {
            completions: vec![
                completion("agent-1", "task-call-1"),
                completion("agent-2", "task-call-2"),
            ],
        };
        let state = state("agent-1", "task-call-1");

        assert!(!fully_consumed(&action, Some(&state)));
        let projection = project(&action, pb::AgentMode::Agent as i32, Some(&state)).unwrap();
        assert!(!projection.context.contains("agent-1"));
        assert!(projection.context.contains("agent-2"));
        assert!(!projection.turn_user.message_id.contains("agent-1"));
        assert!(projection.turn_user.message_id.contains("agent-2"));
    }

    #[test]
    fn consumed_terminal_result_suppresses_a_later_terminal_status() {
        let mut action = pb::BackgroundTaskCompletionAction {
            completions: vec![completion("agent-1", "task-call-1")],
        };
        action.completions[0].status = pb::BackgroundTaskStatus::Error as i32;
        let state = state("agent-1", "task-call-1");

        assert!(fully_consumed(&action, Some(&state)));
    }
}
