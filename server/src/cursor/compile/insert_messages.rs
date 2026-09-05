//! Compiles terminal background-task notifications into append-only runtime events.

use std::collections::BTreeMap;

use prost::Message;

use crate::{
    cursor::protocol::proto::agent::v1 as pb, model::TerminalCompletion, store::BlobId, Error,
    Result,
};

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
    pub completions: Vec<ProjectedCompletion>,
}

#[derive(Debug)]
pub(super) struct ProjectedCompletion {
    pub context: String,
    pub turn_user: pb::UserMessage,
    pub terminal: TerminalCompletion,
}

pub(super) fn project(
    action: &pb::BackgroundTaskCompletionAction,
    mode: i32,
) -> Result<Projection> {
    if action.completions.is_empty() {
        return Err(Error::Protocol(
            "background task completion action contains no completion".into(),
        ));
    }

    let mut completions = BTreeMap::new();
    for completion in &action.completions {
        let Some(projected) = project_completion(completion, mode)? else {
            continue;
        };
        if completions
            .insert(projected.terminal.event_id.clone(), projected)
            .is_some()
        {
            return Err(Error::Protocol(format!(
                "duplicate background task completion lifecycle: {}",
                completion.task_id
            )));
        }
    }
    if completions.is_empty() {
        return Err(Error::Protocol(
            "background task notification contains no finished task".into(),
        ));
    }
    Ok(Projection {
        completions: completions.into_values().collect(),
    })
}

pub(super) fn terminal_completions(
    action: &pb::BackgroundTaskCompletionAction,
) -> Result<Vec<TerminalCompletion>> {
    Ok(project(action, pb::AgentMode::Agent as i32)?
        .completions
        .into_iter()
        .map(|completion| completion.terminal)
        .collect())
}

fn project_completion(
    completion: &pb::BackgroundTaskCompletion,
    mode: i32,
) -> Result<Option<ProjectedCompletion>> {
    let kind = pb::BackgroundTaskKind::try_from(completion.kind).map_err(|_| {
        Error::Protocol(format!("unknown background task kind: {}", completion.kind))
    })?;
    if kind == pb::BackgroundTaskKind::Unspecified {
        return Err(Error::Protocol(format!(
            "background task completion has invalid kind: {}",
            kind.as_str_name()
        )));
    }
    let reason = pb::BackgroundTaskCompletionReason::try_from(completion.reason).map_err(|_| {
        Error::Protocol(format!(
            "unknown background task completion reason: {}",
            completion.reason
        ))
    })?;
    if reason != pb::BackgroundTaskCompletionReason::TaskFinished {
        return Ok(None);
    }
    if completion.task_id.is_empty() || completion.title.is_empty() {
        return Err(Error::Protocol(
            "background task completion requires task_id and title".into(),
        ));
    }
    let agent_id = match kind {
        pb::BackgroundTaskKind::Shell => None,
        pb::BackgroundTaskKind::Subagent => Some(
            completion
                .subagent_id
                .as_deref()
                .filter(|id| !id.is_empty())
                .ok_or_else(|| {
                    Error::Protocol("background subagent completion has no subagent_id".into())
                })?,
        ),
        pb::BackgroundTaskKind::Unspecified => unreachable!(),
    };
    completion
        .tool_call_id
        .as_deref()
        .filter(|id| !id.is_empty())
        .ok_or_else(|| Error::Protocol("background task completion has no tool_call_id".into()))?;
    let status = status(completion)?;
    let kind_name = match kind {
        pb::BackgroundTaskKind::Subagent => "subagent",
        pb::BackgroundTaskKind::Shell => "shell",
        pb::BackgroundTaskKind::Unspecified => unreachable!(),
    };
    let lifecycle = serde_json::to_vec(&(kind_name, completion.task_id.as_str()))?;
    let event_id = format!(
        "background-completed:{}",
        BlobId::digest(&lifecycle).to_base64()
    );
    let terminal = TerminalCompletion {
        task_id: completion.task_id.clone(),
        kind: kind_name.into(),
        status: status_name(status).into(),
        payload_digest: Some(BlobId::digest(&completion.encode_to_vec()).to_base64()),
        event_id: event_id.clone(),
    };
    let follow_up = match kind {
        pb::BackgroundTaskKind::Subagent => FOLLOW_UP,
        pb::BackgroundTaskKind::Shell => SHELL_FOLLOW_UP,
        pb::BackgroundTaskKind::Unspecified => unreachable!(),
    };
    Ok(Some(ProjectedCompletion {
        context: completion_context(completion, kind, agent_id)?,
        turn_user: pb::UserMessage {
            text: follow_up.into(),
            message_id: event_id,
            mode,
            is_simulated_msg: Some(true),
            simulated_msg_reason: Some(pb::SimulatedMsgReason::BackgroundTaskCompletion as i32),
            simulated_message_metadata: Some(pb::user_message::SimulatedMessageMetadata {
                title: Some(completion.title.clone()),
                task_id: Some(completion.task_id.clone()),
                ..Default::default()
            }),
            ..Default::default()
        },
        terminal,
    }))
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

    fn completion(task_id: &str, tool_call_id: &str) -> pb::BackgroundTaskCompletion {
        pb::BackgroundTaskCompletion {
            task_id: task_id.into(),
            kind: pb::BackgroundTaskKind::Subagent as i32,
            status: pb::BackgroundTaskStatus::Success as i32,
            title: format!("Task {task_id}"),
            reason: pb::BackgroundTaskCompletionReason::TaskFinished as i32,
            subagent_id: Some("reused-agent".into()),
            tool_call_id: Some(tool_call_id.into()),
            ..Default::default()
        }
    }

    #[test]
    fn lifecycle_identity_uses_task_id_not_reusable_call_or_agent_ids() {
        let action = pb::BackgroundTaskCompletionAction {
            completions: vec![
                completion("task-1", "reused-call"),
                completion("task-2", "reused-call"),
            ],
        };
        let projection = project(&action, pb::AgentMode::Agent as i32).unwrap();
        assert_eq!(projection.completions.len(), 2);
        assert_ne!(
            projection.completions[0].terminal.event_id,
            projection.completions[1].terminal.event_id
        );
    }

    #[test]
    fn completion_event_id_is_stable_when_another_batch_item_is_consumed() {
        let first = completion("task-1", "call-1");
        let second = completion("task-2", "call-2");
        let batch = project(
            &pb::BackgroundTaskCompletionAction {
                completions: vec![first, second.clone()],
            },
            pb::AgentMode::Agent as i32,
        )
        .unwrap();
        let retry = project(
            &pb::BackgroundTaskCompletionAction {
                completions: vec![second],
            },
            pb::AgentMode::Agent as i32,
        )
        .unwrap();
        let batch_second = batch
            .completions
            .iter()
            .find(|item| item.terminal.task_id == "task-2")
            .unwrap();
        assert_eq!(
            batch_second.terminal.event_id,
            retry.completions[0].terminal.event_id
        );
    }
}
