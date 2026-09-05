//! Coordinates construction of a complete Cursor checkpoint.
use std::collections::HashSet;

use prost::Message;

use crate::{
    cursor::{
        checkpoint::{messages, PendingSteps},
        protocol::proto::agent::v1 as pb,
        services::blob_sync::BlobSynchronizer,
        transport::TransportHandle,
    },
    model::{CanonicalMessage, ToolCall, ToolDefinition, ToolRoundAssistant},
    store::Store,
    Result,
};

use super::{derived, roots::RootFrontier, turns::TurnFrontier};

#[derive(Clone)]
pub struct CheckpointBuilder {
    pub(super) store: Store,
    pub(super) sync: BlobSynchronizer,
    pub(super) parent_tool_call_id: Option<String>,
    pub(super) base: pb::ConversationStateStructure,
    pub(super) model: String,
    pub(super) max_context_tokens: Option<u64>,
    pub(super) instructions: String,
    pub(super) tool_definitions: Vec<ToolDefinition>,
    pub(super) allowed_tools: Vec<String>,
    pub(super) dynamic_tools: HashSet<String>,
    pub(super) turn_user: Option<pb::UserMessage>,
    pub(super) roots: Option<RootFrontier>,
    pub(super) turn: Option<TurnFrontier>,
    pub(super) turns_initialized: bool,
}

impl CheckpointBuilder {
    pub fn new(
        store: Store,
        sync: BlobSynchronizer,
        parent_tool_call_id: Option<String>,
        base: Option<pb::ConversationStateStructure>,
    ) -> Self {
        Self {
            store,
            sync,
            parent_tool_call_id,
            base: base.unwrap_or_default(),
            model: String::new(),
            max_context_tokens: None,
            instructions: String::new(),
            tool_definitions: Vec::new(),
            allowed_tools: Vec::new(),
            dynamic_tools: HashSet::new(),
            turn_user: None,
            roots: None,
            turn: None,
            turns_initialized: false,
        }
    }

    pub fn record_background_completion_action(
        &mut self,
        action: &pb::BackgroundTaskCompletionAction,
    ) {
        for completion in &action.completions {
            if completion.kind != pb::BackgroundTaskKind::Subagent as i32
                || completion.reason != pb::BackgroundTaskCompletionReason::TaskFinished as i32
                || completion.task_id.is_empty()
            {
                continue;
            }
            let Some(tool_call_id) = completion
                .tool_call_id
                .as_deref()
                .filter(|tool_call_id| !tool_call_id.is_empty())
            else {
                continue;
            };
            let Some(run) = self
                .base
                .subagent_runs_by_parent_tool_call_id
                .get_mut(tool_call_id)
            else {
                continue;
            };
            if run.subagent_id.as_deref() != completion.subagent_id.as_deref() {
                continue;
            }
            let status = match pb::BackgroundTaskStatus::try_from(completion.status) {
                Ok(pb::BackgroundTaskStatus::Success) => pb::SubagentRunStatus::Success,
                Ok(pb::BackgroundTaskStatus::Error) => pb::SubagentRunStatus::Error,
                Ok(pb::BackgroundTaskStatus::Aborted) => pb::SubagentRunStatus::Aborted,
                _ => continue,
            };
            run.task_id = Some(completion.task_id.clone());
            run.status = status as i32;
            run.detail = completion.detail.clone();
            run.output_path = completion.output_path.clone();
            run.completed_timestamp_ms = Some(crate::cursor::tools::runtime::now_ms());
            run.completion_reason = Some(pb::BackgroundTaskCompletionReason::TaskFinished as i32);
        }
    }

    pub fn configure(
        &mut self,
        model: String,
        max_context_tokens: Option<u64>,
        instructions: String,
        tool_definitions: Vec<ToolDefinition>,
        dynamic_tools: HashSet<String>,
        turn_user: Option<pb::UserMessage>,
    ) {
        self.model = model;
        self.max_context_tokens = max_context_tokens;
        self.instructions = instructions;
        self.allowed_tools = tool_definitions
            .iter()
            .map(|tool| tool.name.clone())
            .collect();
        self.tool_definitions = tool_definitions;
        self.dynamic_tools = dynamic_tools;
        self.turn_user = turn_user;
    }

    pub(crate) fn record_context_tokens(&mut self, used_tokens: Option<u64>) {
        let previous = self
            .base
            .token_details
            .as_ref()
            .map(|details| details.max_tokens as u64);
        let max_tokens = context_limit(self.max_context_tokens, previous);
        let Some(max_tokens) = max_tokens else {
            return;
        };
        let details = self.base.token_details.get_or_insert_with(Default::default);
        if let Some(used_tokens) = used_tokens {
            details.used_tokens = used_tokens.min(u32::MAX as u64) as u32;
        }
        details.max_tokens = max_tokens.min(u32::MAX as u64) as u32;
        details.prompt_context_usage_tree = None;
        details.prompt_context_usage_snapshot_blob_id = None;
    }

    pub async fn settled(
        &mut self,
        messages: &[CanonicalMessage],
        mode: i32,
        presentation: &PendingSteps,
    ) -> Result<pb::ConversationStateStructure> {
        self.build_state(messages, mode, Vec::new(), presentation)
            .await
    }

    pub async fn staged_tool_round(
        &mut self,
        stable_messages: &[CanonicalMessage],
        mode: i32,
        assistant: &ToolRoundAssistant,
        calls: &[ToolCall],
        started_at_ms: u64,
        presentation: &PendingSteps,
    ) -> Result<pb::ConversationStateStructure> {
        reset_resumed_subagent_runs(&mut self.base, calls);
        let pending = messages::staged_tool_round(
            assistant,
            calls,
            &self.model,
            &self.allowed_tools,
            &self.dynamic_tools,
            started_at_ms,
        )?;
        self.build_state(stable_messages, mode, vec![pending], presentation)
            .await
    }

    pub async fn staged_final(
        &mut self,
        stable_messages: &[CanonicalMessage],
        mode: i32,
        assistant: &CanonicalMessage,
        started_at_ms: u64,
        presentation: &PendingSteps,
    ) -> Result<pb::ConversationStateStructure> {
        let pending = messages::staged_final(
            assistant,
            &self.model,
            &self.allowed_tools,
            &self.dynamic_tools,
            started_at_ms,
        )?;
        self.build_state(stable_messages, mode, vec![pending], presentation)
            .await
    }

    async fn build_state(
        &mut self,
        messages: &[CanonicalMessage],
        mode: i32,
        pending_tool_calls: Vec<String>,
        presentation: &PendingSteps,
    ) -> Result<pb::ConversationStateStructure> {
        self.record_background_subagents(presentation);
        self.record_consumed_subagent_completions(presentation);
        let root_ids = self.project_roots(messages).await?;
        let turn_ids = self.project_turns(mode, presentation).await?;
        let (todo_ids, plan_id) = self.build_derived_state(messages).await?;
        self.base.todos = todo_ids.iter().map(|id| id.as_bytes().to_vec()).collect();
        self.base.plan = plan_id.as_ref().map(|id| id.as_bytes().to_vec());
        let communicate_update_states_by_parent_tool_call_id = self
            .parent_tool_call_id
            .as_ref()
            .and_then(|parent| {
                derived::update_current_step_state(messages).map(|state| (parent.clone(), state))
            })
            .into_iter()
            .collect();

        for path in &presentation.read_paths {
            if !self.base.read_paths.contains(path) {
                self.base.read_paths.push(path.clone());
            }
        }
        let mut checkpoint = self.base.clone();
        checkpoint.root_prompt_messages_json =
            root_ids.iter().map(|id| id.as_bytes().to_vec()).collect();
        checkpoint.turns = turn_ids.iter().map(|id| id.as_bytes().to_vec()).collect();
        checkpoint.pending_tool_calls = pending_tool_calls;
        checkpoint.mode = Some(mode);
        checkpoint.communicate_update_states_by_parent_tool_call_id =
            communicate_update_states_by_parent_tool_call_id;
        if let Some(details) = checkpoint.token_details.as_mut() {
            details.breakdown = Some(crate::cursor::services::usage::breakdown(
                details.used_tokens,
                details.max_tokens,
                details.breakdown.as_ref(),
                &self.instructions,
                &self.tool_definitions,
                &self.dynamic_tools,
                messages,
            )?);
        }
        Ok(checkpoint)
    }

    fn record_background_subagents(&mut self, presentation: &PendingSteps) {
        for step in &presentation.steps {
            let Some(pb::conversation_step::Message::ToolCall(call)) = step.message.as_ref() else {
                continue;
            };
            let Some(pb::tool_call::Tool::TaskToolCall(task)) = call.tool.as_ref() else {
                continue;
            };
            let (Some(args), Some(result)) = (task.args.as_ref(), task.result.as_ref()) else {
                continue;
            };
            let Some(pb::task_result::Result::Success(success)) = result.result.as_ref() else {
                continue;
            };
            if !success.is_background {
                continue;
            }
            let Some(agent_id) = success.agent_id.as_ref().filter(|id| !id.is_empty()) else {
                continue;
            };
            let Some(tool_call_id) = call.tool_call_id.as_ref().filter(|id| !id.is_empty()) else {
                continue;
            };
            let started_at_ms = call
                .started_at_ms
                .unwrap_or_else(crate::cursor::tools::runtime::now_ms);
            let last_used_timestamp_ms = call.completed_at_ms.unwrap_or(started_at_ms);
            self.base
                .subagent_states
                .entry(agent_id.clone())
                .and_modify(|state| state.last_used_timestamp_ms = last_used_timestamp_ms)
                .or_insert_with(|| pb::SubagentPersistedState {
                    conversation_state: None,
                    created_timestamp_ms: started_at_ms,
                    last_used_timestamp_ms,
                    subagent_type: args.subagent_type.clone(),
                    model_id: args.model.clone(),
                    environment: args.environment,
                    cloud_subagent: None,
                    first_class_bc_id: None,
                    cloud_requested_environment_build_id: None,
                    machine: args.machine.clone(),
                });
            self.base.subagent_runs_by_parent_tool_call_id.insert(
                tool_call_id.clone(),
                pb::SubagentRunState {
                    parent_tool_call_id: tool_call_id.clone(),
                    subagent_id: Some(agent_id.clone()),
                    environment: args.environment,
                    status: pb::SubagentRunStatus::Backgrounded as i32,
                    title: Some(args.description.clone()),
                    detail: success.result_suffix.clone(),
                    transcript_path: success.transcript_path.clone(),
                    output_path: None,
                    completed_timestamp_ms: None,
                    completion_reason: None,
                    task_id: Some(agent_id.clone()),
                },
            );
        }
    }

    fn record_consumed_subagent_completions(&mut self, presentation: &PendingSteps) {
        for step in &presentation.steps {
            let Some(pb::conversation_step::Message::ToolCall(call)) = step.message.as_ref() else {
                continue;
            };
            let Some((task_id, status)) = consumed_subagent_completion(call) else {
                continue;
            };
            let Some(state) = self
                .base
                .subagent_runs_by_parent_tool_call_id
                .values_mut()
                .find(|state| state.task_id.as_deref() == Some(task_id))
            else {
                continue;
            };
            state.status = status as i32;
            state.completed_timestamp_ms = call
                .completed_at_ms
                .or_else(|| Some(crate::cursor::tools::runtime::now_ms()));
            state.completion_reason = Some(pb::BackgroundTaskCompletionReason::TaskFinished as i32);
        }
    }

    pub async fn publish(
        &self,
        handle: &TransportHandle,
        checkpoint: &pb::ConversationStateStructure,
    ) -> Result<()> {
        tracing::debug!(
            request_id = self.sync.request_id(),
            stable_roots = checkpoint.root_prompt_messages_json.len(),
            pending_assistants = checkpoint.pending_tool_calls.len(),
            "publishing Cursor checkpoint"
        );
        let result = handle.emit(&pb::AgentServerMessage {
            ttft_breakdown: None,
            message: Some(
                pb::agent_server_message::Message::ConversationCheckpointUpdate(checkpoint.clone()),
            ),
        });
        if let Some(trace) = handle.trace() {
            trace.artifact(
                "checkpoint",
                "byok_server",
                &checkpoint.encode_to_vec(),
                serde_json::json!({
                    "root_message_count": checkpoint.root_prompt_messages_json.len(),
                    "turn_count": checkpoint.turns.len(),
                    "pending_tool_call_count": checkpoint.pending_tool_calls.len(),
                    "emit_status": if result.is_ok() { "sent" } else { "error" },
                }),
            );
        }
        result
    }
}

fn reset_resumed_subagent_runs(state: &mut pb::ConversationStateStructure, calls: &[ToolCall]) {
    for call in calls {
        let normalized = call
            .name
            .chars()
            .filter(|character| character.is_ascii_alphanumeric())
            .flat_map(char::to_lowercase)
            .collect::<String>();
        if !matches!(normalized.as_str(), "task" | "sendmessagetoagent") {
            continue;
        }
        let Some(subagent_id) = call
            .arguments
            .get("resume")
            .or_else(|| call.arguments.get("agent_id"))
            .or_else(|| call.arguments.get("agentId"))
            .and_then(serde_json::Value::as_str)
            .filter(|id| !id.is_empty() && *id != "self")
        else {
            continue;
        };
        let Some(previous) = state
            .subagent_runs_by_parent_tool_call_id
            .values()
            .find(|run| run.subagent_id.as_deref() == Some(subagent_id))
            .cloned()
        else {
            continue;
        };
        state
            .subagent_runs_by_parent_tool_call_id
            .retain(|_, run| run.subagent_id.as_deref() != Some(subagent_id));
        state.subagent_runs_by_parent_tool_call_id.insert(
            call.call_id.clone(),
            pb::SubagentRunState {
                parent_tool_call_id: call.call_id.clone(),
                subagent_id: Some(subagent_id.to_owned()),
                environment: previous.environment,
                status: pb::SubagentRunStatus::Running as i32,
                title: call
                    .arguments
                    .get("description")
                    .and_then(serde_json::Value::as_str)
                    .filter(|title| !title.is_empty())
                    .map(str::to_owned)
                    .or(previous.title),
                detail: None,
                transcript_path: None,
                output_path: None,
                completed_timestamp_ms: None,
                completion_reason: None,
                task_id: Some(subagent_id.to_owned()),
            },
        );
    }
}

fn consumed_subagent_completion(call: &pb::ToolCall) -> Option<(&str, pb::SubagentRunStatus)> {
    match call.tool.as_ref()? {
        pb::tool_call::Tool::AwaitToolCall(tool) => {
            let agent_id = tool.args.as_ref()?.task_id.as_str();
            let status = match tool.result.as_ref()?.result.as_ref()? {
                pb::await_result::Result::Complete(_) => pb::SubagentRunStatus::Success,
                pb::await_result::Result::Error(_) | pb::await_result::Result::StillRunning(_) => {
                    return None
                }
                pb::await_result::Result::Success(success) => {
                    match success.await_result.as_ref()? {
                        pb::await_success::AwaitResult::Complete(_) => {
                            pb::SubagentRunStatus::Success
                        }
                        pb::await_success::AwaitResult::StillRunning(_) => return None,
                    }
                }
            };
            (!agent_id.is_empty()).then_some((agent_id, status))
        }
        pb::tool_call::Tool::TaskToolCall(tool) => {
            let args = tool.args.as_ref()?;
            let result = tool.result.as_ref()?.result.as_ref()?;
            let (agent_id, status) = match result {
                pb::task_result::Result::Success(success) if !success.is_background => (
                    success.agent_id.as_deref().or(args.resume.as_deref())?,
                    pb::SubagentRunStatus::Success,
                ),
                pb::task_result::Result::Error(_) => return None,
                pb::task_result::Result::Success(_) => return None,
            };
            (!agent_id.is_empty()).then_some((agent_id, status))
        }
        _ => None,
    }
}

fn context_limit(selected: Option<u64>, previous: Option<u64>) -> Option<u64> {
    selected.or(previous.filter(|tokens| *tokens != 0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resumed_subagent_starts_a_new_running_lifecycle() {
        let mut state = pb::ConversationStateStructure {
            subagent_runs_by_parent_tool_call_id: std::collections::HashMap::from([(
                "old-task-call".into(),
                pb::SubagentRunState {
                    parent_tool_call_id: "old-task-call".into(),
                    subagent_id: Some("agent-1".into()),
                    status: pb::SubagentRunStatus::Error as i32,
                    title: Some("Old title".into()),
                    detail: Some("stopped".into()),
                    completed_timestamp_ms: Some(42),
                    completion_reason: Some(
                        pb::BackgroundTaskCompletionReason::TaskFinished as i32,
                    ),
                    ..Default::default()
                },
            )]),
            ..Default::default()
        };
        let calls = vec![ToolCall {
            index: 0,
            call_id: "resume-task-call".into(),
            model_call_id: "model-call".into(),
            name: "Task".into(),
            arguments_text: "{}".into(),
            arguments: serde_json::json!({
                "resume": "agent-1",
                "description": "Continue inspection",
            }),
            argument_error: None,
        }];

        reset_resumed_subagent_runs(&mut state, &calls);

        assert!(!state
            .subagent_runs_by_parent_tool_call_id
            .contains_key("old-task-call"));
        let resumed = &state.subagent_runs_by_parent_tool_call_id["resume-task-call"];
        assert_eq!(resumed.subagent_id.as_deref(), Some("agent-1"));
        assert_eq!(resumed.status, pb::SubagentRunStatus::Running as i32);
        assert_eq!(resumed.title.as_deref(), Some("Continue inspection"));
        assert_eq!(resumed.detail, None);
        assert_eq!(resumed.completed_timestamp_ms, None);
        assert_eq!(resumed.completion_reason, None);
    }

    #[test]
    fn completed_await_consumes_the_subagent_terminal_result() {
        let call = pb::ToolCall {
            tool: Some(pb::tool_call::Tool::AwaitToolCall(pb::AwaitToolCall {
                args: Some(pb::AwaitArgs {
                    task_id: "agent-1".into(),
                    ..Default::default()
                }),
                result: Some(pb::AwaitResult {
                    result: Some(pb::await_result::Result::Complete(
                        pb::AwaitTaskComplete::default(),
                    )),
                }),
            })),
            ..Default::default()
        };

        assert_eq!(
            consumed_subagent_completion(&call),
            Some(("agent-1", pb::SubagentRunStatus::Success))
        );
    }

    #[test]
    fn still_running_await_does_not_consume_the_terminal_result() {
        let call = pb::ToolCall {
            tool: Some(pb::tool_call::Tool::AwaitToolCall(pb::AwaitToolCall {
                args: Some(pb::AwaitArgs {
                    task_id: "agent-1".into(),
                    ..Default::default()
                }),
                result: Some(pb::AwaitResult {
                    result: Some(pb::await_result::Result::StillRunning(
                        pb::AwaitTaskStillRunning::default(),
                    )),
                }),
            })),
            ..Default::default()
        };

        assert_eq!(consumed_subagent_completion(&call), None);
    }

    #[test]
    fn failed_await_call_does_not_consume_the_terminal_result() {
        let call = pb::ToolCall {
            tool: Some(pb::tool_call::Tool::AwaitToolCall(pb::AwaitToolCall {
                args: Some(pb::AwaitArgs {
                    task_id: "agent-1".into(),
                    ..Default::default()
                }),
                result: Some(pb::AwaitResult {
                    result: Some(pb::await_result::Result::Error(pb::AwaitError {
                        error: "not found".into(),
                    })),
                }),
            })),
            ..Default::default()
        };

        assert_eq!(consumed_subagent_completion(&call), None);
    }

    #[test]
    fn foreground_resume_consumes_the_existing_background_subagent_result() {
        let call = pb::ToolCall {
            tool: Some(pb::tool_call::Tool::TaskToolCall(pb::TaskToolCall {
                args: Some(pb::TaskArgs {
                    resume: Some("agent-1".into()),
                    ..Default::default()
                }),
                result: Some(pb::TaskResult {
                    result: Some(pb::task_result::Result::Success(pb::TaskSuccess {
                        agent_id: Some("agent-1".into()),
                        is_background: false,
                        ..Default::default()
                    })),
                }),
                ..Default::default()
            })),
            ..Default::default()
        };

        assert_eq!(
            consumed_subagent_completion(&call),
            Some(("agent-1", pb::SubagentRunStatus::Success))
        );
    }

    #[test]
    fn failed_resume_call_does_not_consume_the_background_subagent_terminal_result() {
        let call = pb::ToolCall {
            tool: Some(pb::tool_call::Tool::TaskToolCall(pb::TaskToolCall {
                args: Some(pb::TaskArgs {
                    resume: Some("agent-1".into()),
                    ..Default::default()
                }),
                result: Some(pb::TaskResult {
                    result: Some(pb::task_result::Result::Error(pb::TaskError {
                        error: "resume request failed".into(),
                    })),
                }),
                ..Default::default()
            })),
            ..Default::default()
        };

        assert_eq!(consumed_subagent_completion(&call), None);
    }
}
