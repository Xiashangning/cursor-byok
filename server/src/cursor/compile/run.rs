//! Compiles an AgentRunRequest into a PreparedRun.
use std::collections::BTreeMap;

use uuid::Uuid;

use crate::{
    cursor::prompting::{Mode, PromptCompiler},
    cursor::{
        checkpoint::messages,
        checkpoint::CheckpointBuilder,
        protocol::proto::agent::v1 as pb,
        services::blob_sync::BlobSynchronizer,
        services::context_sync::RequestContextSynchronizer,
        tools::runtime::{is_orchestration_tool, ExecContext, ModelDirectory, SubagentModel},
    },
    model::{
        CanonicalMessage, ContentPart, ConversationId, MessageContent, ModelSpec, ModelVariantAxis,
        ModelVariantParts, Origin, PreparedRun, PromptSpec, Role, RunAction, RunId, RunKind,
    },
    plugin::{PluginModelDescriptor, PluginRegistry},
    store::{BlobId, Store},
    Error, Result,
};

use super::{break_messages, context, insert_messages, model};

struct ActionProjection {
    mode: i32,
    turn_user: Option<pb::UserMessage>,
    action_context: String,
    event_id: Option<String>,
    input_id: Option<String>,
    starts_turn: bool,
    compacting: bool,
    background_completions: Vec<insert_messages::ProjectedCompletion>,
}

pub struct CursorRunContext {
    pub request_id: String,
    pub mode: i32,
    pub turn_user: Option<pb::UserMessage>,
    pub exec: ExecContext,
    pub dynamic_tools: BTreeMap<String, pb::McpToolDefinition>,
    pub checkpoint_prompt: PromptSpec,
    pub compacting: bool,
    pub background_completion: bool,
}

pub(crate) struct PrepareDependencies<'a> {
    pub compiler: &'a PromptCompiler,
    pub store: &'a Store,
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
        plugins,
        checkpoint,
        blob_sync,
        context_sync,
        local_rules_dir,
    } = dependencies;
    checkpoint
        .import_prefetched(&request.pre_fetched_blobs)
        .await?;
    let conversation_id = request_conversation_id(request_id, request);
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
        let encoded = serde_json::to_vec(&summary)?;
        trace.artifact("history_projection", "byok_server", &encoded, summary);
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
    let ActionProjection {
        mode: mode_number,
        mut turn_user,
        action_context,
        mut event_id,
        input_id,
        starts_turn,
        compacting,
        background_completions,
    } = action(request)?;
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
    let checkpoint_mode = if request.subagent_type_name.is_some() {
        Mode::Subagent
    } else {
        mode_from_proto(mode_number)?
    };
    let plugin_models = match plugins {
        Some(plugins) => plugins.configured_models().await,
        None => Vec::new(),
    };
    let mut model = model::requested_model(request)?;
    let requested_model_id = model.model_id.clone();
    let mut selected_axis = None;
    let mut variant_applied = false;
    if let Some(configured_model) = store.resolve_model(&model.model_id).await? {
        model.model_id = configured_model.model_hash.clone();
        configured_model.configure(&mut model);
        if let Some(parts) = configured_model
            .variant_axis()
            .parse_slug(&configured_model.model_hash, &requested_model_id)
        {
            apply_variant_parts(&mut model, parts);
            variant_applied = true;
        }
        selected_axis = Some(configured_model.variant_axis());
    } else if let Some((descriptor, axis, parts)) =
        resolve_plugin_model(&plugin_models, &requested_model_id)
    {
        model.model_id = descriptor.id.clone();
        match parts {
            Some(parts) => {
                apply_variant_parts(&mut model, parts);
                variant_applied = true;
            }
            // 插件模型没有 configure() 兜底:裸 id 按目录默认变体口径填充。
            None => apply_plugin_defaults(&mut model, &axis),
        }
        selected_axis = Some(axis);
    }
    // 显式参数覆盖保存的默认值;根会话里也覆盖所选变体(模型选择器换档)。
    // 子代理的变体 slug 由父代理 Task 调用烘焙,本身就是权威选择,
    // Cursor 回程回声的 parameters 不得改写它。
    if !variant_applied || request.subagent_type_name.is_none() {
        if let Some(requested) = request.requested_model.as_ref() {
            model::apply_requested_parameters(&mut model, requested)?;
        }
    }
    let inherited_subagent_model_variant = selected_axis
        .as_ref()
        .and_then(|axis| model_variant_id(axis, &model.model_id, &model));
    let dynamic = context::dynamic_mcp(request, &request_context)?;
    let subagent_model_overrides = model::overrides(request)?;
    let model_directory = load_model_directory(store, &plugin_models).await?;
    let subagents_disabled = !subagent_model_overrides.is_empty()
        && subagent_model_overrides.iter().all(|(_, selection)| {
            matches!(selection, crate::model::SubagentModelOverride::Disabled)
        });
    let available_subagent_models = if compiler.needs_available_subagent_models(checkpoint_mode) {
        load_available_subagent_models(store, &plugin_models).await?
    } else {
        String::new()
    };
    let mut checkpoint_prompt = compiler.prompt_spec_with_available_subagent_models(
        checkpoint_mode,
        &model,
        &dynamic
            .values()
            .map(|(_, definition)| definition.clone())
            .collect::<Vec<_>>(),
        request.suppress_subagent_progress_update_tool == Some(true),
        &available_subagent_models,
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
        let mut messages = Vec::with_capacity(background_completions.len());
        let mut checkpoint_text = Vec::with_capacity(background_completions.len());
        let mut checkpoint_user = None;
        for projected in background_completions {
            let event_id = projected.terminal.event_id.clone();
            let existing = store
                .message(&conversation_id, &format!("runtime:{event_id}"))
                .await?;
            let (mut message, text) = match existing {
                Some(message) => {
                    let text = runtime_message_text(&message)?;
                    (message, text)
                }
                None => {
                    break_messages::compile_background(
                        event_id,
                        &projected.turn_user,
                        &request_context,
                        &projected.context,
                        blob_sync,
                    )
                    .await?
                }
            };
            message.terminal_completion = Some(projected.terminal);
            checkpoint_text.push(text);
            checkpoint_user.get_or_insert(projected.turn_user);
            messages.push(message);
        }
        if let Some(mut user) = checkpoint_user {
            user.text = checkpoint_text.join("\n\n");
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
    let (base_checkpoint_id, reused) = store
        .match_checkpoint_prefix(&conversation_id, base_checkpoint_id, &initial_messages)
        .await?;
    if !background_completion {
        initial_messages.drain(..reused);
    }
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
    );
    Ok((
        PreparedRun {
            run_id,
            cursor_request_id: Some(request_id.into()),
            conversation_id,
            kind: RunKind::Root,
            model,
            prompt,
            initial_messages,
            action,
            base_checkpoint_id,
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
        },
    ))
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

/// 插件模型没有 configure() 兜底:裸 id 请求按目录默认变体口径填充
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

/// 插件模型的变体轴:没有配置窗口,轴即描述符的生效档位(已并入用户覆盖)。
fn plugin_variant_axis(descriptor: &PluginModelDescriptor) -> ModelVariantAxis {
    ModelVariantAxis {
        context_options: descriptor.context_options.clone(),
        effort_options: descriptor.effort_options.clone(),
    }
}

/// 按精确 id 或变体 slug 解析插件模型;slug 大小写不敏感。
fn resolve_plugin_model<'m>(
    models: &'m [PluginModelDescriptor],
    key: &str,
) -> Option<(
    &'m PluginModelDescriptor,
    ModelVariantAxis,
    Option<ModelVariantParts>,
)> {
    for descriptor in models {
        let axis = plugin_variant_axis(descriptor);
        if descriptor.id == key {
            return Some((descriptor, axis, None));
        }
        if let Some(parts) = axis
            .parse_slug(&descriptor.id, key)
            .or_else(|| axis.parse_slug(&descriptor.id, &key.to_ascii_lowercase()))
        {
            return Some((descriptor, axis, Some(parts)));
        }
    }
    None
}

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

/// 把一个模型(内置或插件)登记进目录:别名(含小写与全部变体 slug)、
/// 变体轴与展示名都以 base key(hash 或插件 id)归组。
fn insert_directory_model(
    directory: &mut ModelDirectory,
    key: &str,
    display_name: &str,
    aliases: &[&str],
    axis: ModelVariantAxis,
) {
    directory
        .display_names
        .insert(key.to_string(), display_name.to_string());
    for alias in aliases {
        directory
            .aliases
            .insert((*alias).to_string(), key.to_string());
        directory
            .aliases
            .insert(alias.to_ascii_lowercase(), key.to_string());
    }
    // 变体 slug 也是别名:继承与显式 slug 选择都能归一到 base key。
    for context in &axis.context_options {
        let efforts: Vec<Option<String>> = if axis.effort_options.is_empty() {
            vec![None]
        } else {
            axis.effort_options
                .iter()
                .map(|effort| Some(effort.clone()))
                .collect()
        };
        for effort in efforts {
            for fast in [false, true] {
                let slug = axis.bake_slug(
                    key,
                    &ModelVariantParts {
                        context: context.clone(),
                        effort: effort.clone(),
                        fast,
                    },
                );
                directory.aliases.insert(slug.clone(), key.to_string());
                directory
                    .aliases
                    .insert(slug.to_ascii_lowercase(), key.to_string());
            }
        }
    }
    directory.variants.insert(key.to_string(), axis);
}

async fn load_model_directory(
    store: &Store,
    plugin_models: &[PluginModelDescriptor],
) -> Result<ModelDirectory> {
    let models = store.models().await?;
    let mut directory = ModelDirectory::default();
    for model in &models {
        insert_directory_model(
            &mut directory,
            &model.model_hash,
            &model.display_name,
            &[&model.model_hash, &model.model_id, &model.display_name],
            model.variant_axis(),
        );
    }
    for descriptor in plugin_models {
        insert_directory_model(
            &mut directory,
            &descriptor.id,
            &descriptor.display_name,
            &[&descriptor.id, &descriptor.display_name],
            plugin_variant_axis(descriptor),
        );
    }
    Ok(directory)
}

/// Task 清单行:展示名 + 生效轴;轴为空时省略该段。内置与插件模型共用。
fn format_subagent_model_line(
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
        format!("- {display_name}")
    } else {
        format!("- {display_name} — {}", segments.join("; "))
    }
}

fn format_available_subagent_model(model: &crate::model::ModelConfig) -> String {
    format_subagent_model_line(
        &model.display_name,
        &model.effort_options,
        &model.context_options,
    )
}

async fn load_available_subagent_models(
    store: &Store,
    plugin_models: &[PluginModelDescriptor],
) -> Result<String> {
    let models = store.models().await?;
    let mut lines = vec!["- inherit".to_string()];
    lines.extend(models.iter().map(format_available_subagent_model));
    lines.extend(plugin_models.iter().map(|descriptor| {
        format_subagent_model_line(
            &descriptor.display_name,
            &descriptor.effort_options,
            &descriptor.context_options,
        )
    }));
    Ok(lines.join("\n"))
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

pub(crate) fn request_conversation_id(
    request_id: &str,
    request: &pb::AgentRunRequest,
) -> ConversationId {
    ConversationId::new(
        request
            .conversation_id
            .clone()
            .unwrap_or_else(|| request_id.into()),
    )
}

pub(crate) fn background_terminal_completions(
    request: &pb::AgentRunRequest,
) -> Result<Option<Vec<crate::model::TerminalCompletion>>> {
    let Some(pb::conversation_action::Action::BackgroundTaskCompletionAction(action)) = request
        .action
        .as_ref()
        .and_then(|action| action.action.as_ref())
    else {
        return Ok(None);
    };
    insert_messages::terminal_completions(action).map(Some)
}

pub(crate) async fn compile_background_action(
    action: &pb::BackgroundTaskCompletionAction,
    mode: i32,
    blobs: &BlobSynchronizer,
    store: &Store,
    conversation_id: &ConversationId,
) -> Result<Vec<CanonicalMessage>> {
    let mut messages = Vec::new();
    for projected in insert_messages::project(action, mode)?.completions {
        let terminal = projected.terminal;
        let mut message = match store
            .message(conversation_id, &format!("runtime:{}", terminal.event_id))
            .await?
        {
            Some(message) => message,
            None => {
                break_messages::compile_background(
                    terminal.event_id.clone(),
                    &projected.turn_user,
                    &pb::RequestContext::default(),
                    &projected.context,
                    blobs,
                )
                .await?
                .0
            }
        };
        message.terminal_completion = Some(terminal);
        messages.push(message);
    }
    Ok(messages)
}

fn execution_run_id(request_id: &str) -> RunId {
    let execution_id = Uuid::new_v4().simple().to_string();
    RunId::new(format!("{request_id}:{}", &execution_id[..8]))
}

fn action(request: &pb::AgentRunRequest) -> Result<ActionProjection> {
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
        });
    };
    match action {
        pb::conversation_action::Action::UserMessageAction(action) => {
            let user = action.user_message.as_ref().ok_or_else(|| {
                Error::Protocol("Cursor user message action has no UserMessage".into())
            })?;
            let mode = if user.mode == pb::AgentMode::Unspecified as i32 {
                mode
            } else {
                user.mode
            };
            if user.message_id.is_empty() {
                return Err(Error::Protocol(
                    "Cursor user message action has no message_id".into(),
                ));
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
            })
        }
        pb::conversation_action::Action::BackgroundTaskCompletionAction(action) => {
            let projection = insert_messages::project(action, mode)?;
            let turn_user = projection
                .completions
                .first()
                .map(|completion| completion.turn_user.clone());
            Ok(ActionProjection {
                mode,
                action_context: String::new(),
                event_id: None,
                input_id: None,
                turn_user,
                starts_turn: true,
                compacting: false,
                background_completions: projection.completions,
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
        pb::AgentMode::Project => Ok(Mode::Projects),
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
    inherited_model_variant: Option<String>,
    model_directory: &ModelDirectory,
    overrides: &[(
        crate::model::SubagentKind,
        crate::model::SubagentModelOverride,
    )],
) -> ExecContext {
    let subagent_models = overrides
        .iter()
        .map(|(kind, value)| {
            let model = match value {
                crate::model::SubagentModelOverride::Explicit(model) => {
                    SubagentModel::Model(override_model_slug(model, model_directory))
                }
                crate::model::SubagentModelOverride::Inherit => SubagentModel::Inherit,
                crate::model::SubagentModelOverride::Disabled => SubagentModel::Disabled,
            };
            (kind.clone(), model)
        })
        .collect();
    ExecContext {
        conversation_id: conversation_id.to_string(),
        root_conversation_id: request
            .conversation_group_id
            .clone()
            .unwrap_or_else(|| conversation_id.to_string()),
        default_subagent_model: model_id.into(),
        default_subagent_model_variant: inherited_model_variant.clone(),
        child_models: request
            .conversation_state
            .as_ref()
            .into_iter()
            .flat_map(|state| &state.subagent_states)
            .filter_map(|(id, state)| {
                state
                    .model_id
                    .as_ref()
                    .map(|model| (id.clone(), model.clone()))
            })
            .collect(),
        child_tool_calls: request
            .conversation_state
            .as_ref()
            .into_iter()
            .flat_map(|state| &state.subagent_runs_by_parent_tool_call_id)
            .filter_map(|(call_id, run)| {
                run.subagent_id
                    .as_ref()
                    .map(|id| (id.clone(), call_id.clone()))
            })
            .collect(),
        model_directory: model_directory.clone(),
        subagent_models,
        allow_subagents: request.subagent_type_name.is_none(),
        terminals_folder: request_context
            .env
            .as_ref()
            .map(|env| env.terminals_folder.clone())
            .unwrap_or_default(),
        admin_command_denylist: request_context.admin_command_denylist.clone(),
        mcp_routes: context::meta_mcp_routes(request_context),
    }
}

/// 用户在设置里为子代理选的模型:保留完整 ModelSpec 的 effort/context/latency,
/// 烘焙成带变体的 slug,使所选档位对子代理生效。未知模型原样透传。
fn override_model_slug(spec: &crate::model::ModelSpec, directory: &ModelDirectory) -> String {
    let (base, parts) = directory.resolve(&spec.model_id);
    let Some(axis) = directory.variants.get(&base) else {
        return spec.model_id.clone();
    };
    let Some(mut effective) = parts.or_else(|| axis.default_parts()) else {
        return base;
    };
    if let Some(tokens) = spec.context_window_tokens {
        if let Some(option) = axis.context_option_for_tokens(tokens) {
            effective.context = option;
        }
    }
    if !axis.effort_options.is_empty() {
        if let Some(effort) = &spec.reasoning.effort {
            let effort = effort.to_ascii_lowercase();
            if axis.effort_options.iter().any(|option| option == &effort) {
                effective.effort = Some(effort);
            }
        }
    }
    effective.fast = effective.fast || spec.latency == crate::model::ModelLatency::Fast;
    axis.bake_slug(&base, &effective)
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
            sort_order: 0,
            display_name: "Configured Model".into(),
            group_name: None,
            model_type: crate::model::ModelType::OpenAi,
            base_url: String::new(),
            use_full_url: false,
            api_key: String::new(),
            tooltip_data: String::new(),
            model_id: "provider-model".into(),
            reasoning_effort: Some("high".into()),
            effort_options: vec!["low".into(), "high".into()],
            context_options: vec!["272k".into(), "1m".into()],
            openai_endpoint: String::new(),
            openai_extra_params_enabled: false,
            openai_extra_params: serde_json::json!({}),
            custom_headers_enabled: false,
            custom_headers: serde_json::json!({}),
            anthropic_extra_params_enabled: false,
            anthropic_extra_params: serde_json::json!({}),
            context_window_tokens: None,
            max_completion_tokens: None,
            anthropic_max_tokens: None,
            anthropic_thinking_effort: None,
            thinking_budget_tokens: None,
            created_at_ms: 0,
            updated_at_ms: 0,
        }
    }

    fn plugin_model() -> PluginModelDescriptor {
        PluginModelDescriptor {
            id: "plugin/codex/gpt-5".into(),
            plugin_id: "codex-plugin".into(),
            plugin_name: "Codex".into(),
            provider_id: "codex".into(),
            model_id: "gpt-5".into(),
            display_name: "GPT-5".into(),
            description: None,
            icon: String::new(),
            provider_type: "openai".into(),
            max_output_tokens: None,
            images: false,
            enabled: true,
            effort_options: vec!["low".into(), "high".into()],
            context_options: vec!["200k".into(), "1m".into()],
        }
    }

    #[test]
    fn plugin_bare_id_fills_catalog_default_variant() {
        let axis = plugin_variant_axis(&plugin_model());
        let mut selected = crate::model::ModelSpec::new("plugin/codex/gpt-5");

        apply_plugin_defaults(&mut selected, &axis);

        assert_eq!(selected.context_window_tokens, Some(200_000));
        assert_eq!(selected.reasoning.effort.as_deref(), Some("high"));
        assert!(selected.reasoning.enabled);
    }

    #[test]
    fn plugin_defaults_keep_explicit_request_values_and_disablement() {
        let axis = plugin_variant_axis(&plugin_model());
        let mut selected = crate::model::ModelSpec::new("plugin/codex/gpt-5");
        selected.context_window_tokens = Some(1_000_000);
        selected.reasoning.effort = Some("low".into());

        apply_plugin_defaults(&mut selected, &axis);

        assert_eq!(selected.context_window_tokens, Some(1_000_000));
        assert_eq!(selected.reasoning.effort.as_deref(), Some("low"));

        let mut disabled = crate::model::ModelSpec::new("plugin/codex/gpt-5");
        disabled.reasoning.explicitly_disabled = true;
        apply_plugin_defaults(&mut disabled, &axis);
        assert_eq!(disabled.reasoning.effort, None);
        assert!(!disabled.reasoning.enabled);
    }

    #[test]
    fn plugin_model_line_lists_the_effective_axes() {
        assert_eq!(
            format_subagent_model_line(
                &plugin_model().display_name,
                &plugin_model().effort_options,
                &plugin_model().context_options,
            ),
            "- GPT-5 — reasoning: low, high; context: 200k, 1m"
        );
    }

    #[test]
    fn resolve_plugin_model_accepts_exact_ids_and_variant_slugs() {
        let models = vec![plugin_model()];
        let (descriptor, _, parts) = resolve_plugin_model(&models, "plugin/codex/gpt-5").unwrap();
        assert_eq!(descriptor.id, "plugin/codex/gpt-5");
        assert!(parts.is_none());

        let (_, _, parts) = resolve_plugin_model(&models, "plugin/codex/gpt-5-1m-low").unwrap();
        let parts = parts.unwrap();
        assert_eq!(parts.context, "1m");
        assert_eq!(parts.effort.as_deref(), Some("low"));
        assert!(!parts.fast);

        let (_, _, parts) =
            resolve_plugin_model(&models, "PLUGIN/CODEX/GPT-5-1M-LOW-FAST").unwrap();
        assert!(parts.unwrap().fast);

        assert!(resolve_plugin_model(&models, "plugin/codex/gpt-5-2m-low").is_none());
        assert!(resolve_plugin_model(&models, "unknown").is_none());
    }

    #[test]
    fn model_directory_resolves_plugin_aliases_and_variant_slugs() {
        let mut directory = ModelDirectory::default();
        let descriptor = plugin_model();
        insert_directory_model(
            &mut directory,
            &descriptor.id,
            &descriptor.display_name,
            &[&descriptor.id, &descriptor.display_name],
            plugin_variant_axis(&descriptor),
        );
        for key in [
            "plugin/codex/gpt-5",
            "gpt-5",
            "GPT-5",
            "plugin/codex/gpt-5-1m-low-fast",
        ] {
            let (base, _) = directory.resolve(key);
            assert_eq!(base, "plugin/codex/gpt-5", "key {key} must resolve");
        }
        let (base, parts) = directory.resolve("plugin/codex/gpt-5-1m-low-fast");
        let parts = parts.unwrap();
        assert_eq!(directory.display_names[&base], "GPT-5");
        assert!(parts.fast);
    }

    #[test]
    fn plugin_inherited_variant_carries_the_selected_axes() {
        let descriptor = plugin_model();
        let mut selected = crate::model::ModelSpec::new("ignored");
        selected.context_window_tokens = Some(1_000_000);
        selected.reasoning.effort = Some("low".into());
        selected.latency = crate::model::ModelLatency::Fast;
        assert_eq!(
            model_variant_id(&plugin_variant_axis(&descriptor), &descriptor.id, &selected),
            Some("plugin/codex/gpt-5-1m-low-fast".into())
        );
    }

    #[test]
    fn available_subagent_model_line_omits_the_hash() {
        let model = configured_model();
        assert_eq!(
            format_available_subagent_model(&model),
            "- Configured Model — reasoning: low, high; context: 272k, 1m"
        );
        assert!(!format_available_subagent_model(&model).contains("abcd1234"));
    }

    #[test]
    fn available_subagent_model_line_omits_empty_option_axes() {
        let mut model = configured_model();
        model.effort_options = Vec::new();
        assert_eq!(
            format_available_subagent_model(&model),
            "- Configured Model — context: 272k, 1m"
        );
        model.context_options = Vec::new();
        assert_eq!(
            format_available_subagent_model(&model),
            "- Configured Model"
        );
    }

    #[test]
    fn inherited_variant_restores_fast_and_skips_the_effort_segment_without_a_reasoning_axis() {
        let mut selected = crate::model::ModelSpec::new("ignored");
        selected.context_window_tokens = Some(1_000_000);
        selected.reasoning.effort = Some("high".into());
        selected.latency = crate::model::ModelLatency::Fast;

        let model = configured_model();
        // 父代理 Fast 时,继承变体携带 -fast,子代理不再静默退回 Standard。
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
        let directory = {
            let model = configured_model();
            let axis = model.variant_axis();
            let mut directory = ModelDirectory::default();
            directory
                .aliases
                .insert(model.model_hash.clone(), model.model_hash.clone());
            directory
                .display_names
                .insert(model.model_hash.clone(), model.display_name.clone());
            directory.variants.insert(model.model_hash.clone(), axis);
            directory
        };
        let mut spec = crate::model::ModelSpec::new("abcd1234");
        spec.context_window_tokens = Some(272_000);
        spec.reasoning.effort = Some("LOW".into());
        spec.latency = crate::model::ModelLatency::Fast;

        // 用户在设置里为子代理选的 effort/context/fast 档烘焙进 slug。
        assert_eq!(
            override_model_slug(&spec, &directory),
            "abcd1234-272k-low-fast"
        );
        let unknown = crate::model::ModelSpec::new("other-model");
        assert_eq!(override_model_slug(&unknown, &directory), "other-model");
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
        assert_eq!(
            mode_from_proto(pb::AgentMode::Project as i32).unwrap(),
            Mode::Projects
        );
        assert!(mode_from_proto(99).is_err());
    }

    #[test]
    fn execution_run_id_keeps_the_request_id_and_adds_eight_uuid_hex_digits() {
        let run_id = execution_run_id("01bba7c5-9c00-4922-b1df-1f58146b5d90");
        let suffix = run_id
            .as_str()
            .strip_prefix("01bba7c5-9c00-4922-b1df-1f58146b5d90:")
            .unwrap();

        assert_eq!(suffix.len(), 8);
        assert!(suffix.bytes().all(|byte| byte.is_ascii_hexdigit()));
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
        let projection = action(&request).unwrap();
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

        let projection = action(&request).unwrap();

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

        let first = action(&request("message-one")).unwrap();
        let second = action(&request("message-two")).unwrap();

        assert_eq!(first.event_id, None);
        assert_eq!(second.event_id, None);
        assert_eq!(first.input_id.as_deref(), Some("cursor:user:message-one"));
        assert_eq!(second.input_id.as_deref(), Some("cursor:user:message-two"));
        assert_ne!(first.input_id, second.input_id);
    }

    #[test]
    fn missing_conversation_id_is_scoped_to_the_request() {
        let request = pb::AgentRunRequest::default();
        assert_eq!(
            request_conversation_id("request-1", &request).as_str(),
            "request-1"
        );
        assert_eq!(
            request_conversation_id("request-2", &request).as_str(),
            "request-2"
        );
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

        let first = action(&request).unwrap();
        let second = action(&request).unwrap();
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
}
