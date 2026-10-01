//! Exposes the extensible Cursor Tool system.
use std::{
    collections::{BTreeMap, HashSet},
    sync::Arc,
};

use tokio::sync::Mutex;

pub mod codec;
pub(crate) mod compat;
mod diagnostics;
pub(crate) mod edit;
pub(crate) mod registry;
pub mod runtime;
mod schedule;
pub(crate) mod stream;
mod tool_call_dispatch;
pub(crate) mod tool_call_result;
mod validation;

use crate::{
    model::{CanonicalMessage, ConversationId, MessageContent, Role, ToolCall},
    search::{WebCache, WebFetch, WebSearch},
    store::Store,
    Error, Result,
};

use self::schedule::{DeferredEdit, EditSchedule};
use self::tool_call_result::{ToolCompletion, ToolResultSender};
use super::protocol::proto::agent::v1 as pb;
use runtime::{task_resume_target, CursorToolRuntime, ExecContext};

#[derive(Clone)]
pub struct ToolDispatcher {
    runtime: CursorToolRuntime,
    results: ToolResultSender,
    search: WebSearch,
    fetch: WebFetch,
    store: Option<Store>,
    edit_schedule: Arc<Mutex<EditSchedule>>,
}

pub struct DispatchedTool {
    pub messages: Vec<pb::AgentServerMessage>,
    pub completion: Option<ToolCompletion>,
}

pub struct ToolBatchState<'a> {
    pub completed: &'a HashSet<String>,
    pub started: &'a HashSet<String>,
    pub response_text: &'a str,
    pub response_thinking: &'a str,
}

pub enum ClientToolEvent {
    Completed(Box<ToolCompletion>),
    Pending,
}

impl ToolDispatcher {
    pub fn new(runtime: CursorToolRuntime) -> Self {
        let (results, _) = tool_call_result::tool_result_channel();
        Self {
            runtime,
            results,
            search: WebSearch::built_in(),
            fetch: WebFetch::built_in(),
            store: None,
            edit_schedule: Arc::new(Mutex::new(EditSchedule::default())),
        }
    }

    pub fn with_results(
        runtime: CursorToolRuntime,
        results: ToolResultSender,
        store: Store,
        web_cache: WebCache,
    ) -> Self {
        Self {
            runtime,
            results,
            search: WebSearch::managed(store.clone()),
            fetch: WebFetch::managed(store.clone(), web_cache),
            store: Some(store),
            edit_schedule: Arc::new(Mutex::new(EditSchedule::default())),
        }
    }

    pub async fn start_batch(
        &self,
        calls: &[ToolCall],
        state: ToolBatchState<'_>,
        messages: &[CanonicalMessage],
        dynamic_mcp: &BTreeMap<String, pb::McpToolDefinition>,
        context: &ExecContext,
    ) -> Result<Vec<DispatchedTool>> {
        let first_tool_index = current_turn_step_count(messages)
            + usize::from(!state.response_thinking.is_empty())
            + usize::from(!state.response_text.is_empty())
            + 1;
        use crate::cursor::prompting::{
            fold_derived_state_from, validated_todo_write, DerivedState,
        };
        let mut todos = fold_derived_state_from(
            messages,
            DerivedState {
                todos: context.initial_todos.clone(),
                plan: None,
            },
        )
        .todos;
        let mut dispatched = Vec::new();
        for (position, call) in calls.iter().enumerate() {
            if state.completed.contains(&call.call_id) {
                continue;
            }
            let message_index = first_tool_index + position;
            let publish_started = !state.started.contains(&call.call_id);
            if let Some(error) = &call.argument_error {
                dispatched.push(validation_failure(call, error.clone()));
                continue;
            }
            if let Err(error) =
                validation::validate(call, &context.tool_definitions).and_then(|()| {
                    if dynamic_mcp.contains_key(&call.name) {
                        Ok(())
                    } else {
                        validation::semantics(call)
                            .and_then(|()| validation::mcp_arguments(call, context))
                    }
                })
            {
                dispatched.push(recover_validation_failure(call, error)?);
                continue;
            }
            if call.name == "TodoWrite" && !dynamic_mcp.contains_key(&call.name) {
                match validated_todo_write(todos.clone(), call.arguments.clone()).and_then(
                    |resolved| {
                        let completion =
                            tool_call_result::todo_write(call, todos.as_ref(), &resolved)?;
                        let mut rendered_call = call.clone();
                        rendered_call.arguments = resolved.clone();
                        let messages = if publish_started {
                            vec![codec::tool_started(&rendered_call, None)?]
                        } else {
                            Vec::new()
                        };
                        todos = Some(resolved);
                        Ok(DispatchedTool {
                            messages,
                            completion: Some(completion),
                        })
                    },
                ) {
                    Ok(result) => dispatched.push(result),
                    Err(error) => dispatched.push(recover_validation_failure(call, error)?),
                }
                continue;
            }
            let edit_path = if dynamic_mcp.contains_key(&call.name) {
                None
            } else {
                match edit::execution_path(call) {
                    Ok(path) => path,
                    Err(error) => {
                        dispatched.push(recover_validation_failure(call, error)?);
                        continue;
                    }
                }
            };
            if let Some(path) = edit_path {
                let next = self.edit_schedule.lock().await.start_or_defer(
                    path,
                    DeferredEdit {
                        call: call.clone(),
                        message_index,
                        publish_started,
                        context: context.clone(),
                    },
                );
                let Some(next) = next else {
                    continue;
                };
                let started = self
                    .start(
                        &next.call,
                        next.message_index,
                        next.publish_started,
                        dynamic_mcp,
                        &next.context,
                        messages,
                    )
                    .await;
                dispatched.push(match started {
                    Ok(started) => started,
                    Err(error) => recover_validation_failure(&next.call, error)?,
                });
                continue;
            }
            let started = self
                .start(
                    call,
                    message_index,
                    publish_started,
                    dynamic_mcp,
                    context,
                    messages,
                )
                .await;
            dispatched.push(match started {
                Ok(started) => started,
                Err(error) => recover_validation_failure(call, error)?,
            });
        }
        Ok(dispatched)
    }

    pub(crate) async fn continue_after(&self, call_id: &str) -> Result<Option<DispatchedTool>> {
        let next = self.edit_schedule.lock().await.complete(call_id)?;
        let Some(next) = next else {
            return Ok(None);
        };
        match self
            .start(
                &next.call,
                next.message_index,
                next.publish_started,
                &BTreeMap::new(),
                &next.context,
                &[],
            )
            .await
        {
            Ok(started) => Ok(Some(started)),
            Err(error) => recover_validation_failure(&next.call, error).map(Some),
        }
    }

    pub async fn interrupt_for_message(&self) -> Vec<u32> {
        self.edit_schedule.lock().await.clear();
        self.runtime.interrupt_for_message().await
    }

    pub(crate) async fn complete_shell_awaits(
        &self,
        action: &pb::BackgroundTaskCompletionAction,
    ) -> Result<Option<tool_call_dispatch::MatchedShellAwaits>> {
        tool_call_dispatch::complete_shell_awaits(&self.runtime, action).await
    }

    async fn start(
        &self,
        call: &ToolCall,
        message_index: usize,
        publish_started: bool,
        dynamic_mcp: &BTreeMap<String, pb::McpToolDefinition>,
        context: &ExecContext,
        history: &[CanonicalMessage],
    ) -> Result<DispatchedTool> {
        let mut resume_variant_write: Option<(String, String)> = None;
        let call = if dynamic_mcp.contains_key(&call.name) {
            call.clone()
        } else {
            let mut normalized = call.clone();
            if call.name != "CallMcpTool" {
                validation::normalize_integers(&mut normalized.arguments);
            }
            let resume_target = task_resume_target(&normalized).map(str::to_owned);
            let resume_model_variant = match (&self.store, resume_target.as_deref()) {
                (Some(store), Some(target)) => {
                    store
                        .conversation_model_variant(&ConversationId::new(target))
                        .await?
                }
                _ => None,
            };
            // 只有显式模型选择才覆盖子会话配置;仅继承父级默认值的续接
            // 不得把它固化进子会话。
            let explicit_model = normalized
                .arguments
                .as_object()
                .is_some_and(runtime::task_resume_has_explicit_model);
            let prepared = context
                .prepare_call_with_resume_variant(&normalized, resume_model_variant.as_deref())?;
            if let (Some(target), Some(model)) = (
                resume_target,
                prepared
                    .arguments
                    .get("model")
                    .and_then(serde_json::Value::as_str),
            ) {
                if explicit_model && resume_model_variant.as_deref() != Some(model) {
                    resume_variant_write = Some((target, model.to_owned()));
                }
            }
            prepared
        };
        let mut messages = if publish_started {
            vec![codec::tool_started(&call, dynamic_mcp.get(&call.name))?]
        } else {
            Vec::new()
        };
        let started = tool_call_dispatch::start(
            &self.runtime,
            &self.results,
            &call,
            message_index,
            dynamic_mcp,
            context,
            self.store.as_ref(),
            history,
        )
        .await?;
        // 派发成功后才写入;失败的派发不留下新配置。消息在此仅被返回,
        // 由调用方发送,因此子会话的首个请求仍会先读到新值。
        if let (Some(store), Some((target, model))) = (self.store.as_ref(), resume_variant_write) {
            store
                .set_conversation_model_variant(&ConversationId::new(target), Some(&model))
                .await?;
        }
        messages.extend(started.messages);
        Ok(DispatchedTool {
            messages,
            completion: started.completion,
        })
    }

    pub async fn interaction_response(
        &self,
        response: &pb::InteractionResponse,
    ) -> Result<ClientToolEvent> {
        if self.runtime.is_interrupted(response.id).await {
            return Ok(ClientToolEvent::Pending);
        }
        let pending = match self.runtime.take_interaction(response.id).await {
            Some(pending) => pending,
            None if self.runtime.completed_call(response.id).await.is_some() => {
                tracing::warn!(
                    id = response.id,
                    "ignoring duplicate terminal interaction response"
                );
                return Ok(ClientToolEvent::Pending);
            }
            None => {
                tracing::warn!(
                    id = response.id,
                    "ignoring response for unknown interaction"
                );
                return Ok(ClientToolEvent::Pending);
            }
        };
        let call = pending.call.clone();
        let continuation = match tool_call_dispatch::resume_interaction(
            &self.results,
            &self.search,
            &self.fetch,
            pending,
            response,
        )
        .await
        {
            Ok(continuation) => continuation,
            Err(Error::Protocol(message)) => {
                return Ok(ClientToolEvent::Completed(Box::new(
                    compat::failure_with_message(&call, message),
                )));
            }
            Err(Error::Json(error)) => {
                return Ok(ClientToolEvent::Completed(Box::new(
                    compat::failure_with_message(&call, error.to_string()),
                )));
            }
            Err(error) => return Err(error),
        };
        Ok(match continuation {
            tool_call_dispatch::InteractionContinuation::Completed(completion) => {
                ClientToolEvent::Completed(completion)
            }
            tool_call_dispatch::InteractionContinuation::Pending => ClientToolEvent::Pending,
        })
    }
}

fn validation_failure(call: &ToolCall, message: String) -> DispatchedTool {
    DispatchedTool {
        messages: Vec::new(),
        completion: Some(compat::failure_with_message(call, message)),
    }
}

fn recover_validation_failure(call: &ToolCall, error: Error) -> Result<DispatchedTool> {
    match error {
        Error::Protocol(message) => Ok(validation_failure(call, message)),
        Error::Json(error) => Ok(validation_failure(call, error.to_string())),
        error => Err(error),
    }
}

fn current_turn_step_count(messages: &[CanonicalMessage]) -> usize {
    let turn_start = messages
        .iter()
        .rposition(|message| message.role == Role::User)
        .map_or(0, |position| position + 1);
    messages[turn_start..]
        .iter()
        .map(|message| match &message.content {
            MessageContent::Assistant {
                text,
                thinking,
                tool_calls,
                ..
            } => {
                usize::from(!thinking.is_empty()) + usize::from(!text.is_empty()) + tool_calls.len()
            }
            _ => 0,
        })
        .sum()
}

#[cfg(test)]
mod shell_await_tests {
    use super::*;
    use serde_json::json;

    fn call(shell_id: &str) -> ToolCall {
        ToolCall {
            index: 0,
            call_id: "await-call".into(),
            model_call_id: "model-call".into(),
            name: "Await".into(),
            arguments_text: json!({"shell_id": shell_id}).to_string(),
            arguments: json!({"shell_id": shell_id}),
            argument_error: None,
        }
    }

    fn action(shell_id: &str) -> pb::BackgroundTaskCompletionAction {
        pb::BackgroundTaskCompletionAction {
            completions: vec![pb::BackgroundTaskCompletion {
                task_id: shell_id.into(),
                kind: pb::BackgroundTaskKind::Shell as i32,
                status: pb::BackgroundTaskStatus::Success as i32,
                title: "Background build".into(),
                output_path: Some(format!("/tmp/{shell_id}.txt")),
                detail: Some("exit_code: 7".into()),
                reason: pb::BackgroundTaskCompletionReason::TaskFinished as i32,
                tool_call_id: Some("shell-call".into()),
                ..Default::default()
            }],
        }
    }

    #[tokio::test]
    async fn shell_completion_finishes_the_matching_pending_await() {
        let runtime = CursorToolRuntime::default();
        let dispatcher = ToolDispatcher::new(runtime.clone());
        assert!(
            dispatcher
                .complete_shell_awaits(&action("99"))
                .await
                .unwrap()
                .is_none(),
            "an unmatched completion stays available for notification delivery"
        );

        let await_call = call("42");
        let pending = runtime
            .reserve_shell_await(&await_call, "42".into())
            .await
            .unwrap();
        runtime
            .reserve_shell_poll(
                &await_call,
                &ExecContext {
                    terminals_folder: "/tmp/terminals".into(),
                    ..ExecContext::default()
                },
                "42",
                pending.started_at_ms,
            )
            .await
            .unwrap()
            .unwrap();

        let matched = dispatcher
            .complete_shell_awaits(&action("42"))
            .await
            .unwrap()
            .unwrap();

        assert_eq!(matched.completions.len(), 1);
        assert_eq!(matched.consumed, vec![("42".into(), "shell-call".into())]);
        assert!(runtime.running_exec_ids().await.is_empty());
        assert!(!matched.completions[0].result().is_error);
        let Some(pb::tool_call::Tool::AwaitToolCall(tool)) =
            &matched.completions[0].tool_call().tool
        else {
            panic!("expected Await tool completion")
        };
        assert!(matches!(
            tool.result.as_ref().and_then(|result| result.result.as_ref()),
            Some(pb::await_result::Result::Success(pb::AwaitSuccess {
                await_result: Some(pb::await_success::AwaitResult::Complete(complete)),
            }))
                if complete.task_id == "42" && complete.exit_code == Some(7)
        ));
    }
}

#[cfg(test)]
mod resumed_task_model_tests {
    use super::*;
    use crate::model::{ConversationId, ModelVariantAxis, SubagentKind};
    use serde_json::json;
    use std::collections::HashMap;

    fn resumed_task() -> ToolCall {
        ToolCall {
            index: 0,
            call_id: "resume-task".into(),
            model_call_id: "model-call".into(),
            name: "Task".into(),
            arguments_text: json!({
                "prompt": "continue",
                "resume": "child-conversation",
                "model_parameters": [{"id": "reasoning", "value": "low"}]
            })
            .to_string(),
            arguments: json!({
                "prompt": "continue",
                "resume": "child-conversation",
                "model_parameters": [{"id": "reasoning", "value": "low"}]
            }),
            argument_error: None,
        }
    }

    #[tokio::test]
    async fn resumed_task_persists_its_effective_model_variant() {
        let store = Store::connect("sqlite::memory:").await.unwrap();
        let child = ConversationId::new("child-conversation");
        store.ensure_conversation(&child).await.unwrap();
        store
            .set_conversation_model_variant(&child, Some("model-hash-1m-high-fast"))
            .await
            .unwrap();
        let (results, _receiver) = tool_call_result::tool_result_channel();
        let dispatcher = ToolDispatcher::with_results(
            CursorToolRuntime::default(),
            results,
            store.clone(),
            WebCache::default(),
        );
        let context = ExecContext {
            default_subagent_model: "model-hash".into(),
            default_subagent_model_variant: Some("model-hash-200k-high".into()),
            model_directory: runtime::ModelDirectory {
                aliases: HashMap::from([("model-hash".into(), "model-hash".into())]),
                variants: HashMap::from([(
                    "model-hash".into(),
                    ModelVariantAxis {
                        context_options: vec!["200k".into(), "1m".into()],
                        effort_options: vec!["low".into(), "high".into()],
                    },
                )]),
                display_names: HashMap::from([("model-hash".into(), "Model".into())]),
            },
            subagent_models: HashMap::from([(
                SubagentKind::GeneralPurpose,
                runtime::SubagentModel::Inherit,
            )]),
            allow_subagents: true,
            ..ExecContext::default()
        };

        let dispatched = dispatcher
            .start(&resumed_task(), 1, false, &BTreeMap::new(), &context, &[])
            .await
            .unwrap();

        assert!(dispatched.completion.is_none());
        assert_eq!(
            store
                .conversation_model_variant(&child)
                .await
                .unwrap()
                .as_deref(),
            Some("model-hash-1m-low-fast")
        );
    }
}
