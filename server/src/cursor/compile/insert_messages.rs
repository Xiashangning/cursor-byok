//! Compiles terminal background-task notifications into append-only runtime events.
//!
//! 每条完成项投影为一条独立的 runtime 消息:身份字段是客户端生成的自由
//! 字符串,proto 不约束字符集(可能含 ':'),冒号切块解析不可靠,因此事件
//! ID 恰好编码一条身份(`background-completed:{kind}:{task}:{tool_call}`),
//! 覆盖检查按事件 ID 精确匹配,无需解析。
use std::collections::{BTreeMap, HashSet};

use crate::{
    cursor::protocol::proto::agent::v1 as pb,
    model::{CanonicalMessage, Role},
    Error, Result,
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

/// 已提交后台完成通知的事件 ID 前缀。
const BACKGROUND_COMPLETED_PREFIX: &str = "background-completed:";

#[derive(Debug)]
pub(super) struct Projection {
    pub completions: Vec<ProjectedCompletion>,
}

#[derive(Debug)]
pub(super) struct ProjectedCompletion {
    pub event_id: String,
    pub context: String,
    pub turn_user: pb::UserMessage,
}

/// 投影仍未完成的完成项。全部滤空(进度通知、台账已消费、或已提交历史中
/// 已覆盖)时返回 Ok(None),表示这是一次无操作的重投。
pub(super) fn project(
    action: &pb::BackgroundTaskCompletionAction,
    mode: i32,
    suppressed: &HashSet<String>,
) -> Result<Option<Projection>> {
    if action.completions.is_empty() {
        return Err(Error::Protocol(
            "background task completion action contains no completion".into(),
        ));
    }

    let mut completions = BTreeMap::new();
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
        let tool_call_id = completion
            .tool_call_id
            .as_deref()
            .filter(|id| !id.is_empty())
            .ok_or_else(|| {
                Error::Protocol("background task completion has no tool_call_id".into())
            })?;
        let identity = format_identity(kind, agent_id.unwrap_or(&completion.task_id), tool_call_id);
        if suppressed.contains(&identity) {
            continue;
        }
        let event_id = background_event_id(&identity);
        let context = completion_context(completion, kind, agent_id)?;
        let follow_up = match kind {
            pb::BackgroundTaskKind::Shell => SHELL_FOLLOW_UP,
            pb::BackgroundTaskKind::Subagent => FOLLOW_UP,
            pb::BackgroundTaskKind::Unspecified => unreachable!(),
        };
        if completions
            .insert(
                event_id.clone(),
                ProjectedCompletion {
                    event_id,
                    context,
                    turn_user: pb::UserMessage {
                        text: follow_up.into(),
                        message_id: background_event_id(&identity),
                        mode,
                        is_simulated_msg: Some(true),
                        simulated_msg_reason: Some(
                            pb::SimulatedMsgReason::BackgroundTaskCompletion as i32,
                        ),
                        simulated_message_metadata: Some(
                            pb::user_message::SimulatedMessageMetadata {
                                title: Some(completion.title.clone()),
                                task_id: Some(completion.task_id.clone()),
                                ..Default::default()
                            },
                        ),
                        ..Default::default()
                    },
                },
            )
            .is_some()
        {
            return Err(Error::Protocol(format!(
                "duplicate background task completion: {identity}"
            )));
        }
    }

    if completions.is_empty() {
        return Ok(None);
    }
    Ok(Some(Projection {
        completions: completions.into_values().collect(),
    }))
}

/// 已覆盖的完成项身份:通知已提交进 base 历史,且其后存在 assistant 回复。
/// 通知已提交但总结缺失(崩溃/取消窗口)不算覆盖,允许重跑 follow-up 自愈。
pub(super) fn covered_identities(
    action: &pb::BackgroundTaskCompletionAction,
    base_messages: &[CanonicalMessage],
) -> HashSet<String> {
    let Some(last_assistant) = base_messages
        .iter()
        .rposition(|message| message.role == Role::Assistant)
    else {
        return HashSet::new();
    };
    let covered_event_ids: HashSet<&str> = base_messages[..last_assistant]
        .iter()
        .filter_map(|message| message.runtime_event_id.as_deref())
        .collect();
    let mut covered = HashSet::new();
    for completion in &action.completions {
        if completion.reason != pb::BackgroundTaskCompletionReason::TaskFinished as i32 {
            continue;
        }
        let Ok(kind) = pb::BackgroundTaskKind::try_from(completion.kind) else {
            continue;
        };
        let Some(identity) = completion_identity(completion, kind) else {
            continue;
        };
        let event_id = background_event_id(&identity);
        if covered_event_ids.contains(event_id.as_str()) {
            covered.insert(identity);
        }
    }
    covered
}

/// 完成项身份:kind + task/agent 身份 + tool_call_id,与事件 ID 中编码的一致。
fn format_identity(
    kind: pb::BackgroundTaskKind,
    task_identity: &str,
    tool_call_id: &str,
) -> String {
    format!("{}:{task_identity}:{tool_call_id}", kind.as_str_name())
}

fn background_event_id(identity: &str) -> String {
    format!("{BACKGROUND_COMPLETED_PREFIX}{identity}")
}

/// 事件 ID 的身份剥离,仅供 round-trip 测试;生产路径按整条事件 ID
/// 精确匹配,从不解析身份字段。
#[cfg(test)]
fn background_event_identity(event_id: &str) -> Option<&str> {
    event_id.strip_prefix(BACKGROUND_COMPLETED_PREFIX)
}

/// 尽力提取完成项身份;字段缺失或非法时返回 None,由 project 负责报错。
fn completion_identity(
    completion: &pb::BackgroundTaskCompletion,
    kind: pb::BackgroundTaskKind,
) -> Option<String> {
    let task_identity = match kind {
        pb::BackgroundTaskKind::Shell => Some(completion.task_id.as_str()),
        pb::BackgroundTaskKind::Subagent => completion.subagent_id.as_deref(),
        pb::BackgroundTaskKind::Unspecified => None,
    }
    .filter(|id| !id.is_empty())?;
    let tool_call_id = completion
        .tool_call_id
        .as_deref()
        .filter(|id| !id.is_empty())?;
    Some(format_identity(kind, task_identity, tool_call_id))
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
    use crate::model::Origin;

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

    fn action(
        completions: Vec<pb::BackgroundTaskCompletion>,
    ) -> pb::BackgroundTaskCompletionAction {
        pb::BackgroundTaskCompletionAction { completions }
    }

    fn notification(agent_id: &str, tool_call_id: &str) -> CanonicalMessage {
        let identity = format_identity(pb::BackgroundTaskKind::Subagent, agent_id, tool_call_id);
        let event_id = background_event_id(&identity);
        let mut message = CanonicalMessage::text(
            format!("runtime:{event_id}"),
            Role::User,
            Origin::Runtime,
            "notification",
        );
        message.runtime_event_id = Some(event_id);
        message
    }

    fn assistant(id: &str) -> CanonicalMessage {
        CanonicalMessage::text(id, Role::Assistant, Origin::Assistant, "summary")
    }

    #[test]
    fn identity_survives_the_event_id_round_trip_even_with_colons() {
        // 身份字段是自由字符串,可能含 ':';事件 ID 只编码一条身份,
        // round-trip 只做前缀剥离,不切块解析。
        let identity = format_identity(
            pb::BackgroundTaskKind::Subagent,
            "agent:with:colons",
            "call:7",
        );
        let event_id = background_event_id(&identity);
        assert_eq!(
            background_event_identity(&event_id),
            Some(identity.as_str())
        );
        assert_eq!(background_event_identity("other:event"), None);
    }

    #[test]
    fn covered_requires_an_assistant_after_the_notification() {
        let action = action(vec![completion("agent-1", "task-call-1")]);
        let identity = format_identity(pb::BackgroundTaskKind::Subagent, "agent-1", "task-call-1");

        // 通知未提交:未覆盖。
        assert!(covered_identities(&action, &[]).is_empty());
        // 通知在尾部、总结缺失(崩溃窗口):未覆盖,允许重跑。
        assert!(covered_identities(&action, &[notification("agent-1", "task-call-1")]).is_empty());
        // assistant 在通知之前:未覆盖。
        assert!(covered_identities(
            &action,
            &[assistant("a"), notification("agent-1", "task-call-1")]
        )
        .is_empty());
        // 通知之后存在 assistant 总结:已覆盖。
        let covered = covered_identities(
            &action,
            &[notification("agent-1", "task-call-1"), assistant("a")],
        );
        assert!(covered.contains(&identity));
    }

    #[test]
    fn covered_matches_complete_ids_before_the_last_assistant() {
        let action = action(vec![completion("agent:1", "call:1")]);
        let expected = HashSet::from([format_identity(
            pb::BackgroundTaskKind::Subagent,
            "agent:1",
            "call:1",
        )]);
        let mut assistant_notification = notification("agent:1", "call:1");
        assistant_notification.role = Role::Assistant;
        for (messages, is_covered) in [
            (
                vec![
                    notification("agent:1", "call:1"),
                    assistant("a"),
                    notification("agent:1", "call:1"),
                ],
                true,
            ),
            (
                vec![
                    assistant("a"),
                    notification("agent:1", "call:1"),
                    notification("agent:1", "call:1"),
                ],
                false,
            ),
            (
                vec![notification("agent:1", "call:10"), assistant("a")],
                false,
            ),
            (vec![assistant_notification.clone()], false),
            (vec![assistant_notification, assistant("a")], true),
        ] {
            let covered = covered_identities(&action, &messages);
            assert_eq!(
                covered,
                if is_covered {
                    expected.clone()
                } else {
                    HashSet::new()
                }
            );
        }
    }

    #[test]
    fn covered_ignores_invalid_and_unfinished_notifications() {
        let mut completions = vec![completion("agent-1", "call-1"); 8];
        completions[0].reason = pb::BackgroundTaskCompletionReason::TaskProgress as i32;
        completions[1].reason = -1;
        completions[2].kind = -1;
        completions[3].kind = pb::BackgroundTaskKind::Unspecified as i32;
        completions[4].subagent_id = None;
        completions[5].subagent_id = Some(String::new());
        completions[6].tool_call_id = None;
        completions[7].tool_call_id = Some(String::new());
        assert!(covered_identities(
            &action(completions),
            &[notification("agent-1", "call-1"), assistant("a")],
        )
        .is_empty());

        let mut shell = completion("shell:1", "call:1");
        shell.kind = pb::BackgroundTaskKind::Shell as i32;
        shell.subagent_id = None;
        let identity = format_identity(pb::BackgroundTaskKind::Shell, "shell:1", "call:1");
        let mut message = notification("shell:1", "call:1");
        message.runtime_event_id = Some(background_event_id(&identity));
        let messages = [message, assistant("a")];
        assert_eq!(
            covered_identities(&action(vec![shell.clone()]), &messages),
            HashSet::from([identity])
        );
        shell.task_id.clear();
        assert!(covered_identities(&action(vec![shell]), &messages).is_empty());
    }

    #[test]
    fn suppressed_or_covered_completions_are_filtered_from_partial_batches() {
        let action = action(vec![
            completion("agent-1", "task-call-1"),
            completion("agent-2", "task-call-2"),
        ]);
        let suppressed = HashSet::from([format_identity(
            pb::BackgroundTaskKind::Subagent,
            "agent-1",
            "task-call-1",
        )]);
        let covered = covered_identities(
            &action,
            &[notification("agent-1", "task-call-1"), assistant("a")],
        );

        for (name, suppressed) in [("suppressed", suppressed), ("covered", covered)] {
            let projection = project(&action, pb::AgentMode::Agent as i32, &suppressed)
                .unwrap()
                .expect("agent-2 remains");

            assert_eq!(projection.completions.len(), 1, "case: {name}");
            let projected = &projection.completions[0];
            assert!(!projected.context.contains("agent-1"), "case: {name}");
            assert!(projected.context.contains("agent-2"), "case: {name}");
            assert!(!projected.event_id.contains("agent-1"), "case: {name}");
            assert!(projected.event_id.contains("agent-2"), "case: {name}");
        }
    }

    #[test]
    fn fully_suppressed_batch_is_a_noop() {
        let action = action(vec![completion("agent-1", "task-call-1")]);
        let suppressed = HashSet::from([format_identity(
            pb::BackgroundTaskKind::Subagent,
            "agent-1",
            "task-call-1",
        )]);

        let projection = project(&action, pb::AgentMode::Agent as i32, &suppressed).unwrap();

        assert!(
            projection.is_none(),
            "a suppressed completion must not reproject"
        );
    }

    #[test]
    fn fully_covered_batch_is_a_noop() {
        let action = action(vec![completion("agent-1", "task-call-1")]);
        let covered = covered_identities(
            &action,
            &[notification("agent-1", "task-call-1"), assistant("a")],
        );

        let projection = project(&action, pb::AgentMode::Agent as i32, &covered).unwrap();

        assert!(
            projection.is_none(),
            "a covered completion must not reproject"
        );
    }

    #[test]
    fn progress_notifications_alone_are_a_noop() {
        let mut progress = completion("agent-1", "task-call-1");
        progress.reason = pb::BackgroundTaskCompletionReason::TaskProgress as i32;
        let action = action(vec![progress]);

        let projection = project(&action, pb::AgentMode::Agent as i32, &HashSet::new()).unwrap();

        assert!(projection.is_none());
    }
}
