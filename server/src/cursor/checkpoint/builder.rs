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
    model::{CanonicalMessage, ConversationId, ToolCall, ToolDefinition, ToolRoundAssistant},
    store::Store,
    Result,
};

use super::{derived, roots::RootFrontier, turns::TurnFrontier};

#[derive(Clone)]
pub struct CheckpointBuilder {
    pub(super) store: Store,
    pub(super) conversation_id: ConversationId,
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

/// A fully built checkpoint plus the completion identities that become safe to
/// suppress only after this exact checkpoint has entered the output stream.
#[derive(Debug)]
pub(crate) struct BuiltCheckpoint {
    pub(crate) state: pb::ConversationStateStructure,
    consumed_background_completions: Vec<(String, String)>,
}

impl BuiltCheckpoint {
    pub(super) fn without_consumptions(state: pb::ConversationStateStructure) -> Self {
        Self {
            state,
            consumed_background_completions: Vec::new(),
        }
    }
}

impl CheckpointBuilder {
    pub fn new(
        store: Store,
        conversation_id: ConversationId,
        sync: BlobSynchronizer,
        parent_tool_call_id: Option<String>,
        base: Option<pb::ConversationStateStructure>,
    ) -> Self {
        Self {
            store,
            conversation_id,
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

    pub(crate) async fn settled(
        &mut self,
        messages: &[CanonicalMessage],
        mode: i32,
        presentation: &PendingSteps,
    ) -> Result<BuiltCheckpoint> {
        self.build_state(messages, mode, Vec::new(), presentation)
            .await
    }

    pub(crate) async fn staged_tool_round(
        &mut self,
        stable_messages: &[CanonicalMessage],
        mode: i32,
        assistant: &ToolRoundAssistant,
        calls: &[ToolCall],
        started_at_ms: u64,
        presentation: &PendingSteps,
    ) -> Result<BuiltCheckpoint> {
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

    pub(crate) async fn staged_final(
        &mut self,
        stable_messages: &[CanonicalMessage],
        mode: i32,
        assistant: &CanonicalMessage,
        started_at_ms: u64,
        presentation: &PendingSteps,
    ) -> Result<BuiltCheckpoint> {
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
    ) -> Result<BuiltCheckpoint> {
        self.record_background_subagents(presentation);
        let consumed_background_completions =
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
        Ok(BuiltCheckpoint {
            state: checkpoint,
            consumed_background_completions,
        })
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
                },
            );
        }
    }

    fn record_consumed_subagent_completions(
        &mut self,
        presentation: &PendingSteps,
    ) -> Vec<(String, String)> {
        let mut consumed = Vec::new();
        for step in &presentation.steps {
            let Some(pb::conversation_step::Message::ToolCall(call)) = step.message.as_ref() else {
                continue;
            };
            let Some((agent_id, status)) = consumed_subagent_completion(call) else {
                continue;
            };
            let Some(state) = self
                .base
                .subagent_runs_by_parent_tool_call_id
                .values_mut()
                .find(|state| state.subagent_id.as_deref() == Some(agent_id))
            else {
                continue;
            };
            state.status = status as i32;
            state.completed_timestamp_ms = call
                .completed_at_ms
                .or_else(|| Some(crate::cursor::tools::runtime::now_ms()));
            state.completion_reason = Some(pb::BackgroundTaskCompletionReason::TaskFinished as i32);
            consumed.push((agent_id.to_owned(), state.parent_tool_call_id.clone()));
        }
        consumed
    }

    pub(crate) async fn publish(
        &self,
        handle: &TransportHandle,
        checkpoint: &BuiltCheckpoint,
    ) -> Result<()> {
        tracing::debug!(
            request_id = self.sync.request_id(),
            stable_roots = checkpoint.state.root_prompt_messages_json.len(),
            pending_assistants = checkpoint.state.pending_tool_calls.len(),
            "publishing Cursor checkpoint"
        );
        let result = handle.emit(&pb::AgentServerMessage {
            ttft_breakdown: None,
            message: Some(
                pb::agent_server_message::Message::ConversationCheckpointUpdate(
                    checkpoint.state.clone(),
                ),
            ),
        });
        if let Some(trace) = handle.trace() {
            trace.artifact(
                "checkpoint",
                "byok_server",
                &checkpoint.state.encode_to_vec(),
                serde_json::json!({
                    "root_message_count": checkpoint.state.root_prompt_messages_json.len(),
                    "turn_count": checkpoint.state.turns.len(),
                    "pending_tool_call_count": checkpoint.state.pending_tool_calls.len(),
                    "emit_status": if result.is_ok() { "sent" } else { "error" },
                }),
            );
        }
        result?;
        self.store
            .record_consumed_background_completions(
                &self.conversation_id,
                pb::BackgroundTaskKind::Subagent.as_str_name(),
                &checkpoint.consumed_background_completions,
            )
            .await
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
                pb::await_result::Result::Error(_) => pb::SubagentRunStatus::Error,
                pb::await_result::Result::StillRunning(_) => return None,
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
    use std::sync::Arc;

    use tokio::sync::mpsc;

    use crate::cursor::{
        services::{blob_sync::BlobSynchronizer, observability::CursorTraceService},
        transport::{OutputHub, TransportHandle},
    };

    use super::*;

    struct BuilderFixture {
        _directory: tempfile::TempDir,
        url: String,
        store: Store,
        conversation_id: ConversationId,
        builder: CheckpointBuilder,
        handle: TransportHandle,
    }

    async fn builder_fixture(
        base: Option<pb::ConversationStateStructure>,
        turn_user: Option<pb::UserMessage>,
    ) -> BuilderFixture {
        let directory = tempfile::tempdir().unwrap();
        let url = format!("sqlite://{}", directory.path().join("test.db").display());
        let store = Store::connect(&url).await.unwrap();
        let conversation_id = ConversationId::new("conversation-1");
        store.ensure_conversation(&conversation_id).await.unwrap();
        let (commands, _receiver) = mpsc::channel(1);
        let output = Arc::new(OutputHub::default());
        let trace = CursorTraceService::new(store.clone()).recorder("request-1");
        let handle = TransportHandle::new("request-1".into(), commands, output, trace);
        let sync = BlobSynchronizer::new("request-1".into(), store.clone(), handle.clone());
        let mut builder =
            CheckpointBuilder::new(store.clone(), conversation_id.clone(), sync, None, base);
        builder.configure(
            "test-model".into(),
            None,
            String::new(),
            Vec::new(),
            HashSet::new(),
            turn_user,
        );
        BuilderFixture {
            _directory: directory,
            url,
            store,
            conversation_id,
            builder,
            handle,
        }
    }

    fn consumed_checkpoint() -> BuiltCheckpoint {
        BuiltCheckpoint {
            state: pb::ConversationStateStructure::default(),
            consumed_background_completions: vec![("agent-1".into(), "task-call-1".into())],
        }
    }

    fn consumed_presentation() -> PendingSteps {
        PendingSteps {
            steps: vec![pb::ConversationStep {
                message: Some(pb::conversation_step::Message::ToolCall(pb::ToolCall {
                    tool_call_id: Some("await-call-1".into()),
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
                })),
            }],
            read_paths: Vec::new(),
        }
    }

    fn completion_action() -> pb::BackgroundTaskCompletionAction {
        pb::BackgroundTaskCompletionAction {
            completions: vec![pb::BackgroundTaskCompletion {
                task_id: "agent-1".into(),
                kind: pb::BackgroundTaskKind::Subagent as i32,
                status: pb::BackgroundTaskStatus::Success as i32,
                title: "Agent result".into(),
                reason: pb::BackgroundTaskCompletionReason::TaskFinished as i32,
                subagent_id: Some("agent-1".into()),
                tool_call_id: Some("task-call-1".into()),
                ..Default::default()
            }],
        }
    }

    #[tokio::test]
    async fn failed_checkpoint_build_does_not_consume_background_completion() {
        let base = pb::ConversationStateStructure {
            subagent_runs_by_parent_tool_call_id: std::collections::HashMap::from([(
                "task-call-1".into(),
                pb::SubagentRunState {
                    parent_tool_call_id: "task-call-1".into(),
                    subagent_id: Some("agent-1".into()),
                    status: pb::SubagentRunStatus::Running as i32,
                    ..Default::default()
                },
            )]),
            ..Default::default()
        };
        let turn_user = pb::UserMessage {
            text: "collect result".into(),
            message_id: "user-1".into(),
            ..Default::default()
        };
        let mut fixture = builder_fixture(Some(base), Some(turn_user)).await;
        fixture.handle.close_output();

        fixture
            .builder
            .settled(&[], pb::AgentMode::Agent as i32, &consumed_presentation())
            .await
            .expect_err("closed output must fail Blob synchronization");

        assert!(fixture
            .store
            .consumed_background_identities(&fixture.conversation_id)
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn failed_checkpoint_publication_does_not_consume_background_completion_after_restart() {
        let BuilderFixture {
            _directory,
            url,
            store,
            conversation_id,
            builder,
            handle,
        } = builder_fixture(None, None).await;
        handle.close_output();
        builder
            .publish(&handle, &consumed_checkpoint())
            .await
            .expect_err("closed output must reject checkpoint publication");
        drop(builder);
        drop(handle);
        drop(store);

        let restarted = Store::connect(&url).await.unwrap();
        let suppressed = restarted
            .consumed_background_identities(&conversation_id)
            .await
            .unwrap();
        assert!(suppressed.is_empty());
        assert!(
            crate::cursor::compile::project_background_completion_for_test(
                &completion_action(),
                &suppressed,
            )
            .unwrap()
        );
    }

    #[tokio::test]
    async fn successful_checkpoint_publication_persists_consumption_once_across_restart() {
        let BuilderFixture {
            _directory,
            url,
            store,
            conversation_id,
            builder,
            handle,
        } = builder_fixture(None, None).await;
        let checkpoint = consumed_checkpoint();
        builder.publish(&handle, &checkpoint).await.unwrap();
        builder.publish(&handle, &checkpoint).await.unwrap();
        drop(builder);
        drop(handle);
        drop(store);

        let restarted = Store::connect(&url).await.unwrap();
        assert_eq!(
            restarted
                .consumed_background_identities(&conversation_id)
                .await
                .unwrap(),
            HashSet::from(["BACKGROUND_TASK_KIND_SUBAGENT:agent-1:task-call-1".to_owned()])
        );
        let row_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM background_consumed WHERE conversation_id = ?",
        )
        .bind(conversation_id.as_str())
        .fetch_one(restarted.pool())
        .await
        .unwrap();
        assert_eq!(row_count, 1);
    }

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
