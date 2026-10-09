//! Compiles an AgentRunRequest into a PreparedRun.
use std::collections::{BTreeMap, HashSet};

use uuid::Uuid;

use crate::{
    cursor::prompting::{Mode, PromptCompiler},
    cursor::{
        checkpoint::messages,
        checkpoint::CheckpointBuilder,
        protocol::proto::agent::v1 as pb,
        services::blob_sync::BlobSynchronizer,
        services::context_sync::RequestContextSynchronizer,
        tools::runtime::{is_orchestration_tool, ExecContext, SubagentModel},
    },
    model::{
        CanonicalMessage, ContentPart, ConversationId, MessageContent, ModelDirectory, ModelSource,
        ModelSpec, ModelVariantAxis, ModelVariantParts, Origin, PreparedRun, PromptSpec,
        Resolution, Role, RunAction, RunId, RunKind,
    },
    plugin::PluginRegistry,
    store::{BlobId, Store},
    Error, Result,
};

use super::{break_messages, context, insert_messages, model};

#[cfg(test)]
use crate::plugin::PluginModelDescriptor;

struct ActionProjection {
    mode: i32,
    turn_user: Option<pb::UserMessage>,
    action_context: String,
    event_id: Option<String>,
    input_id: Option<String>,
    starts_turn: bool,
    compacting: bool,
    background_completions: Vec<insert_messages::ProjectedCompletion>,
    /// 后台完成通知在已提交历史中全部已覆盖:本次投递是无操作重投。
    background_noop: bool,
}

#[derive(Clone)]
pub struct CursorRunContext {
    pub request_id: String,
    pub mode: i32,
    pub turn_user: Option<pb::UserMessage>,
    pub exec: ExecContext,
    pub dynamic_tools: BTreeMap<String, pb::McpToolDefinition>,
    pub checkpoint_prompt: PromptSpec,
    pub compacting: bool,
    pub background_completion: bool,
    pub background_noop: bool,
}

pub(crate) struct PrepareDependencies<'a> {
    pub compiler: &'a PromptCompiler,
    pub store: &'a Store,
    pub parent: Option<&'a crate::cursor::transport::TransportParent>,
    pub plugins: Option<&'a PluginRegistry>,
    pub checkpoint: &'a CheckpointBuilder,
    pub blob_sync: &'a BlobSynchronizer,
    pub context_sync: &'a RequestContextSynchronizer,
    pub local_rules_dir: Option<&'a std::path::Path>,
}

pub(crate) async fn prepare(
    request_id: &str,
    request: &pb::AgentRunRequest,
    dependencies: PrepareDependencies<'_>,
) -> Result<(PreparedRun, CursorRunContext)> {
    let PrepareDependencies {
        compiler,
        store,
        parent,
        plugins,
        checkpoint,
        blob_sync,
        context_sync,
        local_rules_dir,
    } = dependencies;
    checkpoint
        .import_prefetched(&request.pre_fetched_blobs)
        .await?;
    let conversation_id = conversation_key(request, request_id);
    let run_id = execution_run_id(request_id);
    let mut base_messages = if request.conversation_state.is_some() {
        Some(
            checkpoint
                .hydrate_messages(request.conversation_state.as_ref())
                .await?,
        )
    } else {
        None
    };
    if let Some(trace) = blob_sync.trace() {
        let hydrated_messages = base_messages.as_deref().unwrap_or_default();
        let hydrated_images = hydrated_messages
            .iter()
            .map(|message| match &message.content {
                MessageContent::Parts { parts } => parts
                    .iter()
                    .filter(|part| matches!(part, ContentPart::Image { .. }))
                    .count(),
                _ => 0,
            })
            .sum::<usize>();
        let history = request
            .action
            .as_ref()
            .and_then(|action| action.action.as_ref())
            .and_then(|action| match action {
                pb::conversation_action::Action::UserMessageAction(action) => {
                    action.conversation_history.as_ref()
                }
                _ => None,
            });
        let summary = serde_json::json!({
            "checkpoint_root_count": request.conversation_state.as_ref().map_or(0, |state| state.root_prompt_messages_json.len()),
            "checkpoint_turn_count": request.conversation_state.as_ref().map_or(0, |state| state.turns.len()),
            "conversation_history_message_count": history.map_or(0, |history| history.messages.len()),
            "hydrated_message_count": hydrated_messages.len(),
            "hydrated_image_count": hydrated_images,
            "selected_source": "root_prompt_messages_json",
        });
        trace.artifact("history_projection", "byok_server", &[], summary);
    }
    let mut request_context = context::hydrate(request, context_sync).await?;
    if let Some(rules_dir) = local_rules_dir {
        context::merge_local_rules(&mut request_context, rules_dir);
    }
    let request_context = request_context;
    let explicit_resume = matches!(
        request
            .action
            .as_ref()
            .and_then(|action| action.action.as_ref()),
        Some(pb::conversation_action::Action::ResumeAction(_))
    );
    let suppressed = match request
        .action
        .as_ref()
        .and_then(|action| action.action.as_ref())
    {
        // 抑制判定收敛为一条:台账(await/前台已消费) ∨ 历史覆盖(通知已提交
        // 且其后已有 assistant 总结)。覆盖判定基于客户端实际持有的 base 历史。
        Some(pb::conversation_action::Action::BackgroundTaskCompletionAction(action)) => {
            let mut suppressed = insert_messages::covered_identities(
                action,
                base_messages.as_deref().unwrap_or_default(),
            );
            suppressed.extend(
                store
                    .consumed_background_identities(&conversation_id)
                    .await?,
            );
            suppressed
        }
        _ => HashSet::new(),
    };
    let ActionProjection {
        mode: mode_number,
        mut turn_user,
        action_context,
        mut event_id,
        input_id,
        starts_turn,
        compacting,
        background_completions,
        background_noop,
    } = action(request, &suppressed)?;
    let background_completion = !background_completions.is_empty();
    let pending_tool_round = if !starts_turn && !compacting {
        match request
            .conversation_state
            .as_ref()
            .map(|state| state.pending_tool_calls.as_slice())
            .unwrap_or_default()
        {
            [] => None,
            [pending] => Some(messages::decode_pending(pending)?),
            pending => {
                return Err(Error::Protocol(format!(
                    "Cursor resume contains {} pending assistant messages",
                    pending.len()
                )))
            }
        }
    } else {
        None
    };
    if let (Some(messages), Some(pending)) = (base_messages.as_mut(), pending_tool_round.as_ref()) {
        messages.extend(pending.completed_messages.iter().cloned());
    }
    let checkpoint_mode = if request.subagent_type_name.is_some() {
        Mode::Subagent
    } else {
        mode_from_proto(mode_number)?
    };
    let plugin_models = match plugins {
        Some(plugins) => plugins.configured_models().await,
        None => Vec::new(),
    };
    let configured_models = store.models().await?;
    let model_directory = ModelDirectory::new(&configured_models, &plugin_models)?;
    let mut model = model::requested_model(request)?;
    let requested_model_id = model.model_id.clone();
    let subagent_conversation_exists = if request.subagent_type_name.is_some() {
        store.conversation_exists(&conversation_id).await?
    } else {
        false
    };
    let stored_model_variant = if subagent_conversation_exists {
        store.conversation_model_selection(&conversation_id).await?
    } else {
        None
    };
    let mut selected_axis = None;
    let effective_selection = match stored_model_variant.as_ref() {
        Some(saved) => saved.clone(),
        None => crate::model::ModelSelection::resolve(&model_directory, &requested_model_id)?,
    };
    model.model_id = effective_selection.id().to_owned();
    if let crate::model::ModelSelection::Local { id, parameters } = &effective_selection {
        let parts = parameters
            .context
            .as_ref()
            .map(|context| ModelVariantParts {
                context: context.clone(),
                effort: parameters.reasoning.clone(),
                fast: parameters.fast.unwrap_or(false),
            });
        let entry = model_directory.get(id).ok_or_else(|| {
            Error::Config(format!(
                "model '{id}' no longer exists; use an available model ID"
            ))
        })?;
        if entry.is_draft() {
            return Err(Error::Config(format!(
                "model '{id}' is a draft: missing model_id"
            )));
        }
        model.model_id = id.clone();
        match entry.source {
            ModelSource::Builtin => {
                let configured_model = configured_models
                    .iter()
                    .find(|candidate| candidate.model_hash == *id)
                    .expect("builtin directory entry matches a configured model");
                configured_model.configure(&mut model);
            }
            ModelSource::Plugin => {
                // 插件模型没有 configure() 兜底:裸 id 按轴默认变体(显式默认,否则轴第一项)填充。
                if parts.is_none() {
                    if let Some(plugin) = entry.plugin.as_ref() {
                        model.display_name = Some(plugin.display_name.clone());
                        if let Some(tokens) = plugin.max_output_tokens {
                            model.max_output_tokens.get_or_insert(tokens);
                        }
                    }
                }
            }
        }
        let axis = entry.axis.clone();
        if let Some(parts) = parts {
            apply_variant_parts(&mut model, parts);
        } else if entry.source == ModelSource::Plugin {
            apply_plugin_defaults(&mut model, &axis);
        }
        selected_axis = Some(axis);
    }
    let parameters = effective_selection.parameters();
    if let Some(reasoning) = &parameters.reasoning {
        model.reasoning.explicitly_disabled = matches!(reasoning.as_str(), "none" | "off");
        model.reasoning.effort = (!model.reasoning.explicitly_disabled).then(|| reasoning.clone());
        model.reasoning.enabled = model.reasoning.effort.is_some();
    }
    if let Some(fast) = parameters.fast {
        model.latency = if fast {
            crate::model::ModelLatency::Fast
        } else {
            crate::model::ModelLatency::Standard
        };
    }
    // 首次子代理请求以 Task 烘焙的变体 slug 为准。续接请求以会话中
    // 已保存的最新配置为准；Task.resume 在派发配置覆盖时先更新该值，
    // 因而 Cursor 回传的裸模型或旧参数不会把新配置改回去。
    // 根会话的参数始终代表模型选择器的当前选择。
    if request.subagent_type_name.is_none()
        || (stored_model_variant.is_none()
            && !matches!(
                model_directory.resolve(&requested_model_id),
                Resolution::Matched { parts: Some(_), .. }
            ))
    {
        if let Some(requested) = request.requested_model.as_ref() {
            model::apply_requested_parameters(&mut model, requested)?;
        }
    }
    let mut inherited_selection = effective_selection.clone();
    if let Some(axis) = selected_axis.as_ref() {
        let parameters = inherited_selection.parameters_mut();
        parameters.context = model
            .context_window_tokens
            .and_then(|tokens| axis.context_option_for_tokens(tokens));
        // 无 effort 轴的模型不携带 reasoning 分量:烘焙进 slug 会让自己的
        // 解析器拒绝(effort 段不被空轴接受),存储的选择也应与轴一致。
        parameters.reasoning = if axis.effort_options.is_empty() {
            None
        } else if model.reasoning.explicitly_disabled {
            Some("none".into())
        } else {
            model.reasoning.effort.clone()
        };
        parameters.fast = Some(model.latency == crate::model::ModelLatency::Fast);
    }
    let inherited_subagent_model_variant = Some(inherited_selection);
    let dynamic = context::dynamic_mcp(request, &request_context)?;
    let subagent_model_overrides = model::overrides(request)?;
    let subagents_disabled = !subagent_model_overrides.is_empty()
        && subagent_model_overrides.iter().all(|(_, selection)| {
            matches!(selection, crate::model::SubagentModelOverride::Disabled)
        });
    let available_subagent_models = if compiler.needs_available_subagent_models(checkpoint_mode) {
        available_subagent_models(&model_directory)
    } else {
        String::new()
    };
    let catalog_reference = if available_subagent_models.is_empty() {
        ""
    } else {
        "See the latest <available_subagent_models> message. Use inherit to keep the parent selection."
    };
    let mut checkpoint_prompt = compiler.prompt_spec_with_available_subagent_models(
        checkpoint_mode,
        &model,
        &dynamic
            .values()
            .map(|(_, definition)| definition.clone())
            .collect::<Vec<_>>(),
        request.suppress_subagent_progress_update_tool == Some(true),
        catalog_reference,
    )?;
    if context::is_remote_ssh(request, &request_context) {
        remove_local_semble_tools(&mut checkpoint_prompt);
    }
    if subagents_disabled {
        checkpoint_prompt
            .tools
            .retain(|tool| !is_orchestration_tool(&tool.name));
    }
    let prompt = if compacting {
        compiler.prompt_spec_with_available_subagent_models(
            Mode::Compaction,
            &model,
            &[],
            false,
            &available_subagent_models,
        )?
    } else {
        checkpoint_prompt.clone()
    };
    let proposed_base_checkpoint_id = match base_messages.as_mut() {
        Some(messages) if !messages.is_empty() => {
            validate_prompt_root(messages)?;
            messages.retain(|message| {
                !(message.role == Role::System && message.origin == Origin::Prompt)
            });
            store.import_checkpoint(&conversation_id, messages).await?
        }
        Some(_) | None => store.ensure_conversation(&conversation_id).await?,
    };
    // 只有首次子任务运行负责初始化会话配置;已存在的子会话(含续接)不得
    // 把继承的父级模型写成它的配置,显式选择由 Task 派发持久化。
    if request.subagent_type_name.is_some() && !subagent_conversation_exists {
        store
            .set_conversation_model_selection(
                &conversation_id,
                inherited_subagent_model_variant.as_ref(),
            )
            .await?;
    }
    let base_checkpoint_id = match input_id.as_deref() {
        Some(input_id) => {
            store
                .anchor_input(&conversation_id, input_id, proposed_base_checkpoint_id)
                .await?
        }
        None => proposed_base_checkpoint_id,
    };
    let mut projected_user_context = if input_id.is_some() && !compacting && !background_completion
    {
        break_messages::compile_request_context(
            "identity",
            &request_context,
            base_messages.as_deref().unwrap_or_default(),
        )?
    } else {
        None
    };
    if event_id.is_none() {
        if let (Some(input_id), Some(user)) = (input_id.as_deref(), turn_user.as_ref()) {
            event_id = Some(
                break_messages::user_event_id(
                    input_id,
                    checkpoint_mode,
                    user,
                    &request_context,
                    &action_context,
                    projected_user_context
                        .as_ref()
                        .map(|message| &message.content),
                    compiler,
                    blob_sync,
                )
                .await?,
            );
        }
    }
    let existing_runtime = match event_id.as_deref() {
        Some(event_id) => {
            store
                .message(&conversation_id, &format!("runtime:{event_id}"))
                .await?
        }
        _ => None,
    };
    let request_context_message = match event_id.as_deref() {
        Some(event_id) if !compacting && !background_completion => {
            let message_id = format!("request-context:{event_id}");
            match store.message(&conversation_id, &message_id).await? {
                Some(message) => Some(message),
                None if input_id.is_some() => projected_user_context.take().map(|mut message| {
                    message.message_id = message_id;
                    message
                }),
                None => break_messages::compile_request_context(
                    event_id,
                    &request_context,
                    base_messages.as_deref().unwrap_or_default(),
                )?,
            }
        }
        _ => None,
    };
    let mut initial_messages = if compacting {
        Vec::new()
    } else if background_completion {
        let base = base_messages.as_deref().unwrap_or_default();
        let mut messages = Vec::with_capacity(background_completions.len());
        let mut texts = Vec::with_capacity(background_completions.len());
        let mut checkpoint_user = None::<pb::UserMessage>;
        for projected in background_completions {
            let existing = store
                .message(&conversation_id, &format!("runtime:{}", projected.event_id))
                .await?;
            let (message, text) = match existing {
                Some(message) => {
                    let text = runtime_message_text(&message)?;
                    (message, text)
                }
                None => {
                    let mut completion_context = projected.context.clone();
                    if let Some(agent_id) = projected.agent_id.as_ref() {
                        if let Some(selection) = store
                            .conversation_model_selection(&ConversationId::new(agent_id))
                            .await?
                        {
                            completion_context.push_str(&format!(
                                "\nModel ID: {}\nModel parameters: {}",
                                selection.id(),
                                serde_json::to_string(selection.parameters())?
                            ));
                        }
                    }
                    break_messages::compile_background(
                        projected.event_id.clone(),
                        &projected.turn_user,
                        &request_context,
                        &completion_context,
                        blob_sync,
                    )
                    .await?
                }
            };
            checkpoint_user.get_or_insert(projected.turn_user);
            texts.push(text);
            // 通知已在 base 历史中(崩溃/取消窗口的重投):历史里已有,
            // 无需重新追加,重跑只为补上缺失的总结。
            if !base.iter().any(|message| {
                message.runtime_event_id.as_deref() == Some(projected.event_id.as_str())
            }) {
                messages.push(message);
            }
        }
        if let Some(mut user) = checkpoint_user {
            user.text = texts.join("\n\n");
            turn_user = Some(user);
        }
        messages
    } else {
        match (turn_user.clone(), event_id) {
            (Some(user), Some(event_id)) => {
                let runtime = match existing_runtime {
                    Some(message) => message,
                    None => {
                        break_messages::compile(
                            event_id,
                            checkpoint_mode,
                            &user,
                            &request_context,
                            &action_context,
                            compiler,
                            blob_sync,
                        )
                        .await?
                    }
                };
                request_context_message
                    .into_iter()
                    .chain(std::iter::once(runtime))
                    .collect()
            }
            (None, None) => Vec::new(),
            _ => {
                return Err(Error::Protocol(
                    "Cursor action has an incomplete runtime event".into(),
                ))
            }
        }
    };
    if explicit_resume {
        initial_messages.extend(break_messages::compile_resume_messages(
            request_id,
            &request_context,
            base_messages.as_deref().unwrap_or_default(),
            pending_tool_round.is_some(),
        )?);
    }
    if !compacting && !background_completion && !available_subagent_models.is_empty() {
        let loaded_history;
        let history: &[CanonicalMessage] = match base_messages.as_deref() {
            Some(messages) => messages,
            None => {
                loaded_history = store.load_current_messages(&conversation_id).await?;
                &loaded_history
            }
        };
        let text = format!("<available_subagent_models>\n{available_subagent_models}\n</available_subagent_models>");
        let event = format!("model-directory:{request_id}");
        let id = format!("runtime:{event}");
        let message = match store.message(&conversation_id, &id).await? {
            Some(message) => message,
            None => {
                let mut message = CanonicalMessage::text(id, Role::User, Origin::Runtime, text);
                message.runtime_event_id = Some(event);
                message
            }
        };
        if history
            .iter()
            .rev()
            .find(|message| message.message_id.starts_with("runtime:model-directory:"))
            .is_none_or(|previous| previous.content != message.content)
        {
            initial_messages.insert(0, message);
        }
    }
    let (base_checkpoint_id, reused) = store
        .match_checkpoint_prefix(&conversation_id, base_checkpoint_id, &initial_messages)
        .await?;
    initial_messages.drain(..reused);
    let action = if compacting {
        RunAction::Compact
    } else if starts_turn {
        RunAction::Start
    } else {
        RunAction::Resume { pending_tool_round }
    };
    let exec = exec_context(
        request,
        &request_context,
        &conversation_id,
        &model.model_id,
        inherited_subagent_model_variant.clone(),
        &model_directory,
        &subagent_model_overrides,
        prompt.tools.clone(),
        checkpoint.base_todo_state().await?,
    )?;
    Ok((
        PreparedRun {
            run_id,
            cursor_request_id: Some(request_id.into()),
            conversation_id,
            kind: run_kind(store, parent, request.subagent_type_name.as_deref()).await?,
            model,
            prompt,
            initial_messages,
            action,
            base_checkpoint_id,
            background_follow_up: background_completion,
        },
        CursorRunContext {
            request_id: request_id.into(),
            mode: mode_number,
            turn_user,
            exec,
            dynamic_tools: dynamic
                .into_iter()
                .map(|(name, (wire, _))| (name, wire))
                .collect(),
            checkpoint_prompt,
            compacting,
            background_completion,
            background_noop,
        },
    ))
}

/// Transport headers identify the dispatch; the request body identifies the type.
/// Missing local parent history must not turn a child into a root or prevent its run.
async fn run_kind(
    store: &Store,
    parent: Option<&crate::cursor::transport::TransportParent>,
    subagent_type: Option<&str>,
) -> Result<RunKind> {
    if parent.is_none() && subagent_type.is_none() {
        return Ok(RunKind::Root);
    }
    let dispatch = match parent {
        Some(parent) => {
            store
                .parent_tool_call_run(&parent.request_id, &parent.tool_call_id)
                .await?
        }
        None => None,
    };
    let arguments = dispatch.as_ref().map(|(_, arguments)| arguments);
    let kind = subagent_type
        .filter(|name| !name.is_empty())
        .or_else(|| arguments?.get("subagent_type")?.as_str())
        .filter(|name| !name.is_empty())
        .unwrap_or("generalPurpose");
    Ok(RunKind::Subagent {
        kind: crate::cursor::tools::runtime::subagent_kind(kind),
        background: arguments
            .and_then(|arguments| arguments.get("run_in_background"))
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false),
        parent_run_id: dispatch.map(|(run_id, _)| run_id),
        parent_tool_call_id: parent.map(|parent| parent.tool_call_id.clone()),
    })
}

/// 把变体 slug 解析出的档位应用到 ModelSpec;两个模型来源(内置/插件)共用。
/// slug 烘焙时三个轴(context/effort/fast)都已确定,应用时全部钉死,
/// 使子代理路径可以安全地跳过回程 parameters 的二次应用。
fn apply_variant_parts(model: &mut ModelSpec, parts: ModelVariantParts) {
    if let Some(tokens) = crate::model::parse_token_count(&parts.context) {
        model.context_window_tokens = Some(tokens);
    }
    if let Some(effort) = parts.effort {
        model.reasoning.explicitly_disabled = matches!(effort.as_str(), "none" | "off");
        model.reasoning.enabled = !model.reasoning.explicitly_disabled;
        model.reasoning.effort = model.reasoning.enabled.then_some(effort);
    }
    model.latency = if parts.fast {
        crate::model::ModelLatency::Fast
    } else {
        crate::model::ModelLatency::Standard
    };
}

/// 插件模型没有 configure() 兜底:裸 id 请求按轴默认变体(显式默认,否则轴第一项)填充
/// context/effort;请求已显式携带的值与显式关闭的 reasoning 优先。
fn apply_plugin_defaults(model: &mut ModelSpec, axis: &ModelVariantAxis) {
    let Some(defaults) = axis.default_parts() else {
        return;
    };
    if model.context_window_tokens.is_none() {
        model.context_window_tokens = crate::model::parse_token_count(&defaults.context);
    }
    if model.reasoning.explicitly_disabled || model.reasoning.effort.is_some() {
        return;
    }
    model.reasoning.effort = defaults.effort;
    model.reasoning.enabled |= model.reasoning.effort.is_some();
}

#[cfg(test)]
fn model_variant_id(axis: &ModelVariantAxis, base: &str, selected: &ModelSpec) -> Option<String> {
    let context = axis.context_option_for_tokens(selected.context_window_tokens?)?;
    let effort = if axis.effort_options.is_empty() {
        None
    } else {
        Some(if selected.reasoning.explicitly_disabled {
            "none".into()
        } else {
            selected.reasoning.effort.clone()?
        })
    };
    Some(axis.bake_slug(
        base,
        &ModelVariantParts {
            context,
            effort,
            fast: selected.latency == crate::model::ModelLatency::Fast,
        },
    ))
}

/// Task 清单行:`{id} ({展示名})` + 生效轴;轴为空时省略该段。
/// id 是调用参数,展示名用于匹配用户指令中的模型名。
fn format_subagent_model_line(
    id: &str,
    display_name: &str,
    effort_options: &[String],
    context_options: &[String],
) -> String {
    let mut segments = Vec::new();
    if !effort_options.is_empty() {
        segments.push(format!("reasoning: {}", effort_options.join(", ")));
    }
    if !context_options.is_empty() {
        segments.push(format!("context: {}", context_options.join(", ")));
    }
    if segments.is_empty() {
        format!("- {id} ({display_name})")
    } else {
        format!("- {id} ({display_name}): {}", segments.join("; "))
    }
}

/// 顺序稳定:目录条目按 ID 排序(内置与插件统一),inherit 保持首行。
fn available_subagent_models(directory: &ModelDirectory) -> String {
    let mut lines = vec!["- inherit".to_string()];
    lines.extend(
        directory
            .entries()
            .filter(|entry| !entry.is_draft())
            .map(|entry| {
                format_subagent_model_line(
                    &entry.id,
                    &entry.display_name,
                    &entry.axis.effort_options,
                    &entry.axis.context_options,
                )
            }),
    );
    lines.join("\n")
}

fn runtime_message_text(message: &CanonicalMessage) -> Result<String> {
    let MessageContent::Parts { parts } = &message.content else {
        return Err(Error::Protocol(
            "stored runtime message does not contain parts".into(),
        ));
    };
    let Some(ContentPart::Text { text }) = parts.first() else {
        return Err(Error::Protocol(
            "stored runtime message does not start with text".into(),
        ));
    };
    Ok(text.clone())
}

fn validate_prompt_root(messages: &[CanonicalMessage]) -> Result<()> {
    let prompts = messages
        .iter()
        .filter(|message| message.role == Role::System && message.origin == Origin::Prompt)
        .collect::<Vec<_>>();
    let [prompt] = prompts.as_slice() else {
        return Err(Error::Protocol(format!(
            "Cursor history contains {} system prompt roots",
            prompts.len()
        )));
    };
    let MessageContent::Parts { parts } = &prompt.content else {
        return Err(Error::Protocol(
            "Cursor system prompt root is not textual content".into(),
        ));
    };
    let [ContentPart::Text { .. }] = parts.as_slice() else {
        return Err(Error::Protocol(
            "Cursor system prompt root is not one text part".into(),
        ));
    };
    Ok(())
}

pub(crate) fn execution_run_id(request_id: &str) -> RunId {
    let execution_id = Uuid::new_v4().simple().to_string();
    RunId::new(format!("{request_id}:{}", &execution_id[..8]))
}

/// 会话键的唯一解析:请求缺 conversation_id 时回退到请求 id。
/// 台账读取方(prepare)与所有写入方(runtime/builder)必须共用此函数,
/// 否则空键会话的抑制判定失效。
pub(crate) fn conversation_key(request: &pb::AgentRunRequest, request_id: &str) -> ConversationId {
    ConversationId::new(
        request
            .conversation_id
            .clone()
            .unwrap_or_else(|| request_id.into()),
    )
}

fn action(request: &pb::AgentRunRequest, suppressed: &HashSet<String>) -> Result<ActionProjection> {
    let conversation_mode = request
        .conversation_state
        .as_ref()
        .and_then(|state| state.mode);
    let mode = conversation_mode.unwrap_or(pb::AgentMode::Agent as i32);
    let Some(action) = request
        .action
        .as_ref()
        .and_then(|action| action.action.as_ref())
    else {
        return Ok(ActionProjection {
            mode,
            turn_user: None,
            action_context: String::new(),
            event_id: None,
            input_id: None,
            starts_turn: false,
            compacting: false,
            background_completions: Vec::new(),
            background_noop: false,
        });
    };
    match action {
        pb::conversation_action::Action::UserMessageAction(action) => {
            let mut user = action.user_message.clone().ok_or_else(|| {
                Error::Protocol("Cursor user message action has no UserMessage".into())
            })?;
            let mode = if user.mode == pb::AgentMode::Unspecified as i32 {
                conversation_mode.unwrap_or(user.mode)
            } else {
                user.mode
            };
            if user.message_id.is_empty() {
                // Cursor CLI omits the initial subagent message ID. Its run ID
                // stays the same across retries, so it can anchor one input.
                if let (Some(_), Some(run_id)) = (
                    request
                        .subagent_type_name
                        .as_deref()
                        .filter(|name| !name.is_empty()),
                    request.run_id.as_deref().filter(|id| !id.is_empty()),
                ) {
                    user.message_id = run_id.into();
                } else {
                    return Err(Error::Protocol(
                        "Cursor user message action has no message_id".into(),
                    ));
                }
            }
            if user.text.trim() == "/summarize" {
                return Ok(ActionProjection {
                    mode,
                    turn_user: Some(user.clone()),
                    action_context: String::new(),
                    event_id: None,
                    input_id: None,
                    starts_turn: false,
                    compacting: true,
                    background_completions: Vec::new(),
                    background_noop: false,
                });
            }
            let mut context = action
                .prepend_user_messages
                .iter()
                .map(|message| message.text.trim())
                .filter(|text| !text.is_empty())
                .map(str::to_string)
                .collect::<Vec<_>>();
            context.extend(
                user.subagent_system_reminder
                    .iter()
                    .filter(|text| !text.is_empty())
                    .cloned(),
            );
            let input_id = format!("cursor:user:{}", user.message_id);
            Ok(ActionProjection {
                mode,
                turn_user: Some(user.clone()),
                action_context: context.join("\n\n"),
                event_id: None,
                input_id: Some(input_id),
                starts_turn: true,
                compacting: false,
                background_completions: Vec::new(),
                background_noop: false,
            })
        }
        pb::conversation_action::Action::BackgroundTaskCompletionAction(action) => {
            let projection = insert_messages::project(action, mode, suppressed)?;
            let (completions, noop) = match projection {
                Some(projection) => (projection.completions, false),
                // 全部完成项已被消费或覆盖:无操作重投。
                None => (Vec::new(), true),
            };
            Ok(ActionProjection {
                mode,
                action_context: String::new(),
                event_id: None,
                input_id: None,
                turn_user: None,
                starts_turn: true,
                compacting: false,
                background_completions: completions,
                background_noop: noop,
            })
        }
        pb::conversation_action::Action::ExecutePlanAction(action) => execute_plan(action),
        pb::conversation_action::Action::SummarizeAction(_) => Ok(ActionProjection {
            mode,
            turn_user: None,
            action_context: String::new(),
            event_id: None,
            input_id: None,
            starts_turn: false,
            compacting: true,
            background_completions: Vec::new(),
            background_noop: false,
        }),
        _ => Ok(ActionProjection {
            mode,
            turn_user: None,
            action_context: String::new(),
            event_id: None,
            input_id: None,
            starts_turn: false,
            compacting: false,
            background_completions: Vec::new(),
            background_noop: false,
        }),
    }
}

fn execute_plan(action: &pb::ExecutePlanAction) -> Result<ActionProjection> {
    let plan = action
        .plan_file_content
        .as_deref()
        .or_else(|| action.plan.as_ref().map(|plan| plan.plan.as_str()))
        .filter(|plan| !plan.trim().is_empty())
        .ok_or_else(|| Error::Protocol("ExecutePlan is missing plan content".into()))?;
    let source = action
        .plan_file_uri
        .as_deref()
        .or(action.plan_file_path.as_deref())
        .filter(|source| !source.is_empty());
    let action_context = match source {
        Some(source) => {
            format!("<approved_plan>\n<plan_file>{source}</plan_file>\n{plan}\n</approved_plan>")
        }
        None => format!("<approved_plan>\n{plan}\n</approved_plan>"),
    };
    let identity = BlobId::digest(
        format!(
            "{}\0{}\0{}\0{}\0{}",
            action.execution_mode,
            action.plan_id.as_deref().unwrap_or_default(),
            action.kickoff_message_id.as_deref().unwrap_or_default(),
            source.unwrap_or_default(),
            plan,
        )
        .as_bytes(),
    )
    .to_base64();
    let event_id = format!("execute-plan:{identity}");
    Ok(ActionProjection {
        mode: action.execution_mode,
        turn_user: Some(pb::UserMessage {
            text: "Execute the approved plan.".into(),
            message_id: event_id.clone(),
            mode: action.execution_mode,
            ..Default::default()
        }),
        action_context,
        event_id: Some(event_id),
        input_id: None,
        starts_turn: true,
        compacting: false,
        background_completions: Vec::new(),
        background_noop: false,
    })
}

pub(super) fn mode_from_proto(mode: i32) -> Result<Mode> {
    let mode = pb::AgentMode::try_from(mode)
        .map_err(|_| Error::Protocol(format!("unknown Cursor agent mode: {mode}")))?;
    match mode {
        pb::AgentMode::Agent => Ok(Mode::Agent),
        pb::AgentMode::Ask => Ok(Mode::Ask),
        pb::AgentMode::Plan => Ok(Mode::Plan),
        pb::AgentMode::Debug => Ok(Mode::Debug),
        pb::AgentMode::Multitask => Ok(Mode::Multitask),
        mode => Err(Error::Protocol(format!(
            "unsupported Cursor agent mode: {}",
            mode.as_str_name()
        ))),
    }
}

fn exec_context(
    request: &pb::AgentRunRequest,
    request_context: &pb::RequestContext,
    conversation_id: &ConversationId,
    model_id: &str,
    inherited_model_variant: Option<crate::model::ModelSelection>,
    model_directory: &ModelDirectory,
    overrides: &[(
        crate::model::SubagentKind,
        crate::model::SubagentModelOverride,
    )],
    tool_definitions: Vec<crate::model::ToolDefinition>,
    initial_todos: serde_json::Value,
) -> Result<ExecContext> {
    let subagent_models = overrides
        .iter()
        .map(|(kind, value)| {
            let model = match value {
                crate::model::SubagentModelOverride::Explicit(model) => {
                    SubagentModel::Model(override_model_selection(model, model_directory)?)
                }
                crate::model::SubagentModelOverride::Inherit => SubagentModel::Inherit,
                crate::model::SubagentModelOverride::Disabled => SubagentModel::Disabled,
            };
            Ok((kind.clone(), model))
        })
        .collect::<Result<_>>()?;
    Ok(ExecContext {
        tool_definitions,
        initial_todos: Some(initial_todos),
        conversation_id: conversation_id.to_string(),
        root_conversation_id: request
            .conversation_group_id
            .clone()
            .unwrap_or_else(|| conversation_id.to_string()),
        default_subagent_model: model_id.into(),
        default_subagent_model_variant: inherited_model_variant.clone(),
        model_directory: model_directory.clone(),
        subagent_models,
        allow_subagents: request.subagent_type_name.is_none(),
        terminals_folder: request_context
            .env
            .as_ref()
            .map(|env| env.terminals_folder.clone())
            .unwrap_or_default(),
        mcp_routes: context::meta_mcp_routes(request_context),
    })
}

/// Preserve independent parameter overrides without storing Cursor wire slugs.
fn override_model_selection(
    spec: &crate::model::ModelSpec,
    directory: &ModelDirectory,
) -> Result<crate::model::ModelSelection> {
    let mut selection = crate::model::ModelSelection::resolve(directory, &spec.model_id)?;
    let axis = directory.get(selection.id()).map(|entry| &entry.axis);
    let parameters = selection.parameters_mut();
    if let Some(tokens) = spec.context_window_tokens {
        parameters.context = axis.and_then(|axis| axis.context_option_for_tokens(tokens));
    }
    if let Some(effort) = &spec.reasoning.effort {
        parameters.reasoning = Some(match axis {
            Some(axis) => axis.validate_effort(effort)?,
            None => effort.clone(),
        });
    }
    if spec.latency == crate::model::ModelLatency::Fast {
        parameters.fast = Some(true);
    }
    Ok(selection)
}

fn remove_local_semble_tools(prompt: &mut PromptSpec) {
    prompt
        .tools
        .retain(|tool| !matches!(tool.name.as_str(), "SembleSearch" | "SembleFindRelated"));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conversation_key_falls_back_to_the_request_id() {
        let mut request = pb::AgentRunRequest {
            conversation_id: Some("conversation-1".into()),
            ..Default::default()
        };
        assert_eq!(
            conversation_key(&request, "request-1").as_str(),
            "conversation-1"
        );
        request.conversation_id = None;
        assert_eq!(
            conversation_key(&request, "request-1").as_str(),
            "request-1"
        );
        request.conversation_id = Some(String::new());
        assert_eq!(
            conversation_key(&request, "request-1").as_str(),
            "",
            "an explicit empty id is preserved as sent"
        );
    }

    #[test]
    fn remote_prompt_uses_cursor_mcp_instead_of_local_semble() {
        let mut prompt = PromptSpec {
            instructions: String::new(),
            tools: ["SembleSearch", "SembleFindRelated", "CallMcpTool"]
                .into_iter()
                .map(|name| crate::model::ToolDefinition {
                    name: name.into(),
                    description: String::new(),
                    parameters: serde_json::json!({"type": "object"}),
                })
                .collect(),
        };

        remove_local_semble_tools(&mut prompt);

        assert_eq!(
            prompt
                .tools
                .iter()
                .map(|tool| tool.name.as_str())
                .collect::<Vec<_>>(),
            ["CallMcpTool"]
        );
    }

    fn configured_model() -> crate::model::ModelConfig {
        crate::model::ModelConfig {
            model_hash: "abcd1234".into(),
            display_name: "Configured Model".into(),
            model_id: "provider-model".into(),
            default_effort: Some("high".into()),
            effort_options: vec!["low".into(), "high".into()],
            context_options: vec!["272k".into(), "1m".into()],
            ..Default::default()
        }
    }

    fn plugin_model() -> PluginModelDescriptor {
        PluginModelDescriptor {
            id: "07929241".into(),
            plugin_id: "codex-plugin".into(),
            plugin_name: "Codex".into(),
            provider_id: "codex".into(),
            model_id: "org/gpt-5".into(),
            display_name: "GPT-5".into(),
            provider_type: "openai".into(),
            enabled: true,
            effort_options: vec!["low".into(), "high".into()],
            context_options: vec!["200k".into(), "1m".into()],
            ..Default::default()
        }
    }

    #[test]
    fn plugin_bare_id_fills_the_first_axis_variant() {
        let axis = plugin_model().variant_axis();
        let mut selected = crate::model::ModelSpec::new("07929241");

        apply_plugin_defaults(&mut selected, &axis);

        assert_eq!(selected.context_window_tokens, Some(200_000));
        assert_eq!(selected.reasoning.effort.as_deref(), Some("low"));
        assert!(selected.reasoning.enabled);
    }

    #[test]
    fn plugin_defaults_keep_explicit_request_values_and_disablement() {
        let axis = plugin_model().variant_axis();
        let mut selected = crate::model::ModelSpec::new("07929241");
        selected.context_window_tokens = Some(1_000_000);
        selected.reasoning.effort = Some("low".into());

        apply_plugin_defaults(&mut selected, &axis);

        assert_eq!(selected.context_window_tokens, Some(1_000_000));
        assert_eq!(selected.reasoning.effort.as_deref(), Some("low"));

        let mut disabled = crate::model::ModelSpec::new("07929241");
        disabled.reasoning.explicitly_disabled = true;
        apply_plugin_defaults(&mut disabled, &axis);
        assert_eq!(disabled.reasoning.effort, None);
        assert!(!disabled.reasoning.enabled);
    }

    #[test]
    fn drafts_are_not_advertised_or_dispatched_as_task_models() {
        let mut draft = configured_model();
        draft.model_id.clear();
        let directory = ModelDirectory::new(std::slice::from_ref(&draft), &[]).unwrap();
        assert!(directory.contains(&draft.model_hash));
        assert_eq!(available_subagent_models(&directory), "- inherit");
        let context = crate::cursor::tools::runtime::ExecContext {
            model_directory: directory,
            allow_subagents: true,
            ..Default::default()
        };
        let arguments = serde_json::json!({"model": draft.model_hash, "prompt": "work", "description": "Draft"});
        let call = crate::model::ToolCall {
            index: 0,
            call_id: "draft-task".into(),
            model_call_id: "parent".into(),
            name: "Task".into(),
            arguments_text: arguments.to_string(),
            arguments,
            argument_error: None,
        };
        assert!(context
            .prepare_call_with_resume_variant(&call, None)
            .unwrap_err()
            .to_string()
            .contains("missing model_id"));
    }

    #[test]
    fn plugin_inherited_variant_carries_the_selected_axes() {
        let descriptor = plugin_model();
        let mut selected = crate::model::ModelSpec::new("ignored");
        selected.context_window_tokens = Some(1_000_000);
        selected.reasoning.effort = Some("low".into());
        selected.latency = crate::model::ModelLatency::Fast;
        assert_eq!(
            model_variant_id(&descriptor.variant_axis(), &descriptor.id, &selected),
            Some("07929241-1m-low-fast".into())
        );
    }

    #[test]
    fn available_subagent_model_line_leads_with_the_unified_id() {
        let model = configured_model();
        let directory = ModelDirectory::new(std::slice::from_ref(&model), &[]).unwrap();
        assert_eq!(
            available_subagent_models(&directory),
            "- inherit\n- abcd1234 (Configured Model): reasoning: low, high; context: 272k, 1m"
        );
    }

    #[test]
    fn available_subagent_model_line_omits_empty_option_axes() {
        let mut model = configured_model();
        model.effort_options = Vec::new();
        let directory = ModelDirectory::new(std::slice::from_ref(&model), &[]).unwrap();
        assert_eq!(
            available_subagent_models(&directory),
            "- inherit\n- abcd1234 (Configured Model): context: 272k, 1m"
        );
        model.context_options = Vec::new();
        let directory = ModelDirectory::new(std::slice::from_ref(&model), &[]).unwrap();
        assert_eq!(
            available_subagent_models(&directory),
            "- inherit\n- abcd1234 (Configured Model)"
        );
    }

    #[test]
    fn available_subagent_models_are_ordered_stably_across_sources() {
        let mut plugin = plugin_model();
        plugin.id = "00000001".into();
        let directory = ModelDirectory::new(&[configured_model()], &[plugin]).unwrap();
        assert_eq!(
            available_subagent_models(&directory),
            "- inherit\n- 00000001 (GPT-5): reasoning: low, high; context: 200k, 1m\n\
             - abcd1234 (Configured Model): reasoning: low, high; context: 272k, 1m"
        );
    }

    #[test]
    fn inherited_variant_restores_fast_and_skips_the_effort_segment_without_a_reasoning_axis() {
        let mut selected = crate::model::ModelSpec::new("ignored");
        selected.context_window_tokens = Some(1_000_000);
        selected.reasoning.effort = Some("high".into());
        selected.latency = crate::model::ModelLatency::Fast;

        let model = configured_model();
        // 父代理 Fast 时,继承变体携带 -fast。
        assert_eq!(
            model_variant_id(&model.variant_axis(), &model.model_hash, &selected),
            Some("abcd1234-1m-high-fast".into())
        );

        let mut without_effort = configured_model();
        without_effort.effort_options = Vec::new();
        assert_eq!(
            model_variant_id(
                &without_effort.variant_axis(),
                &without_effort.model_hash,
                &selected
            ),
            Some("abcd1234-1m-fast".into())
        );
    }

    #[test]
    fn override_model_slug_bakes_the_configured_variant() {
        let model = configured_model();
        let directory = ModelDirectory::new(std::slice::from_ref(&model), &[]).unwrap();
        let mut spec = crate::model::ModelSpec::new("abcd1234");
        spec.context_window_tokens = Some(272_000);
        spec.reasoning.effort = Some("LOW".into());
        spec.latency = crate::model::ModelLatency::Fast;

        // 用户在设置里为子代理选的 effort/context/fast 档烘焙进 slug。
        assert_eq!(
            override_model_selection(&spec, &directory)
                .unwrap()
                .cursor_model_id(&directory),
            "abcd1234-272k-low-fast"
        );
        let unknown = crate::model::ModelSpec::new("other-model");
        assert_eq!(
            override_model_selection(&unknown, &directory)
                .unwrap()
                .cursor_model_id(&directory),
            "other-model"
        );
    }

    #[test]
    fn restored_system_root_is_structural_not_bound_to_the_next_model() {
        let prompt = CanonicalMessage::text(
            "root",
            Role::System,
            Origin::Prompt,
            "prompt from the previous model",
        );
        validate_prompt_root(std::slice::from_ref(&prompt)).unwrap();
        assert!(validate_prompt_root(&[prompt.clone(), prompt]).is_err());
    }

    #[test]
    fn unsupported_cursor_mode_is_not_silently_treated_as_agent() {
        assert_eq!(
            mode_from_proto(pb::AgentMode::Agent as i32).unwrap(),
            Mode::Agent
        );
        assert!(mode_from_proto(pb::AgentMode::Project as i32).is_err());
        assert!(mode_from_proto(99).is_err());
    }

    #[test]
    fn current_user_message_consumes_the_mode_instead_of_history_mode() {
        let request = pb::AgentRunRequest {
            conversation_state: Some(pb::ConversationStateStructure {
                mode: Some(pb::AgentMode::Agent as i32),
                ..Default::default()
            }),
            action: Some(pb::ConversationAction {
                action: Some(pb::conversation_action::Action::UserMessageAction(
                    pb::UserMessageAction {
                        user_message: Some(pb::UserMessage {
                            text: "explain".into(),
                            message_id: "user-message".into(),
                            mode: pb::AgentMode::Ask as i32,
                            ..Default::default()
                        }),
                        ..Default::default()
                    },
                )),
                ..Default::default()
            }),
            ..Default::default()
        };
        let projection = action(&request, &HashSet::new()).unwrap();
        assert_eq!(projection.mode, pb::AgentMode::Ask as i32);
        assert_eq!(
            projection.input_id.as_deref(),
            Some("cursor:user:user-message")
        );
        assert_eq!(mode_from_proto(projection.mode).unwrap(), Mode::Ask);
    }

    #[test]
    fn queued_user_message_without_mode_inherits_conversation_mode() {
        let request = pb::AgentRunRequest {
            conversation_state: Some(pb::ConversationStateStructure {
                mode: Some(pb::AgentMode::Agent as i32),
                ..Default::default()
            }),
            action: Some(pb::ConversationAction {
                action: Some(pb::conversation_action::Action::UserMessageAction(
                    pb::UserMessageAction {
                        user_message: Some(pb::UserMessage {
                            text: "queued follow-up".into(),
                            message_id: "queued-user-message".into(),
                            ..Default::default()
                        }),
                        ..Default::default()
                    },
                )),
                ..Default::default()
            }),
            ..Default::default()
        };

        let projection = action(&request, &HashSet::new()).unwrap();

        assert_eq!(projection.mode, pb::AgentMode::Agent as i32);
        assert_eq!(mode_from_proto(projection.mode).unwrap(), Mode::Agent);
    }

    #[test]
    fn queued_messages_keep_distinct_input_anchors_until_runtime_identity_is_compiled() {
        let request = |message_id: &str| pb::AgentRunRequest {
            action: Some(pb::ConversationAction {
                action: Some(pb::conversation_action::Action::UserMessageAction(
                    pb::UserMessageAction {
                        user_message: Some(pb::UserMessage {
                            text: "queued follow-up".into(),
                            message_id: message_id.into(),
                            mode: pb::AgentMode::Agent as i32,
                            ..Default::default()
                        }),
                        ..Default::default()
                    },
                )),
                ..Default::default()
            }),
            ..Default::default()
        };

        let first = action(&request("message-one"), &HashSet::new()).unwrap();
        let second = action(&request("message-two"), &HashSet::new()).unwrap();

        assert_eq!(first.event_id, None);
        assert_eq!(second.event_id, None);
        assert_eq!(first.input_id.as_deref(), Some("cursor:user:message-one"));
        assert_eq!(second.input_id.as_deref(), Some("cursor:user:message-two"));
        assert_ne!(first.input_id, second.input_id);
    }

    #[test]
    fn execute_plan_appends_the_approved_plan_as_a_stable_runtime_event() {
        let execute = pb::ExecutePlanAction {
            plan_file_uri: Some("file:///workspace/example.plan.md".into()),
            plan_file_content: Some("# Build\n\n- implement it".into()),
            execution_mode: pb::AgentMode::Agent as i32,
            ..Default::default()
        };
        let request = pb::AgentRunRequest {
            action: Some(pb::ConversationAction {
                action: Some(pb::conversation_action::Action::ExecutePlanAction(
                    execute.clone(),
                )),
                ..Default::default()
            }),
            ..Default::default()
        };

        let first = action(&request, &HashSet::new()).unwrap();
        let second = action(&request, &HashSet::new()).unwrap();
        assert_eq!(first.mode, pb::AgentMode::Agent as i32);
        assert!(first.starts_turn);
        assert_eq!(first.event_id, second.event_id);
        assert_eq!(first.input_id, None);
        assert_eq!(
            first.turn_user.as_ref().map(|user| user.text.as_str()),
            Some("Execute the approved plan.")
        );
        assert!(first
            .action_context
            .contains("file:///workspace/example.plan.md"));
        assert!(first.action_context.contains("# Build\n\n- implement it"));
    }

    #[test]
    fn execute_plan_requires_content() {
        let result = execute_plan(&pb::ExecutePlanAction {
            execution_mode: pb::AgentMode::Agent as i32,
            ..Default::default()
        });
        assert!(matches!(
            result,
            Err(Error::Protocol(message)) if message.contains("missing plan content")
        ));
    }

    fn user_message_request(
        subagent_type_name: Option<&str>,
        run_id: Option<&str>,
        message_id: &str,
    ) -> pb::AgentRunRequest {
        pb::AgentRunRequest {
            subagent_type_name: subagent_type_name.map(str::to_string),
            run_id: run_id.map(str::to_string),
            action: Some(pb::ConversationAction {
                action: Some(pb::conversation_action::Action::UserMessageAction(
                    pb::UserMessageAction {
                        user_message: Some(pb::UserMessage {
                            text: "Inspect the workspace".into(),
                            message_id: message_id.into(),
                            mode: pb::AgentMode::Agent as i32,
                            ..Default::default()
                        }),
                        ..Default::default()
                    },
                )),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[test]
    fn subagent_message_without_id_uses_stable_client_run_id() {
        let request = user_message_request(Some("explore"), Some("child-run-1"), "");
        for _ in 0..2 {
            let projection =
                action(&request, &HashSet::new()).expect("subagent request should start");
            assert_eq!(projection.turn_user.unwrap().message_id, "child-run-1");
            assert_eq!(
                projection.input_id.as_deref(),
                Some("cursor:user:child-run-1")
            );
        }
    }

    #[test]
    fn missing_root_message_id_is_still_rejected() {
        let request = user_message_request(None, Some("root-run-1"), "");
        assert!(action(&request, &HashSet::new()).is_err());
    }

    #[test]
    fn subagent_without_message_or_run_id_is_rejected() {
        let request = user_message_request(Some("explore"), None, "");
        assert!(action(&request, &HashSet::new()).is_err());
    }

    #[test]
    fn supplied_subagent_message_id_is_preserved() {
        let request = user_message_request(Some("explore"), Some("child-run-1"), "message-1");
        assert_eq!(
            action(&request, &HashSet::new())
                .unwrap()
                .turn_user
                .unwrap()
                .message_id,
            "message-1"
        );
    }
}
