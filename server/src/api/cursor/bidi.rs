//! Accepts ordered Cursor Bidi append requests and routes them by request_id.
use prost::Message;

use crate::{
    cursor::{
        conversation::TransportCommand,
        protocol::{
            events,
            proto::{agent::v1 as agent, aiserver::v1 as ai},
        },
        transport::{TransportParent, TransportRegistry},
    },
    model::{ModelDirectory, Resolution},
    Error, Result,
};

pub struct DecodedAppend {
    pub request_id: String,
    pub seqno: i64,
    pub message: agent::AgentClientMessage,
    /// resolve_model_selection 是否改写了消息内容。官方上游转发据此决定
    /// 重编码还是原样转发原始字节:prost 往返会丢弃本仓库 proto 之外的字段,
    /// 未改写的消息必须原样转发。
    pub rewritten: bool,
    /// 客户端实际使用的载荷表示。重编码必须回到同一表示:hex 请求得到 hex
    /// 重编码,binary 请求得到 binary 重编码,不混用。
    pub encoding: AppendEncoding,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AppendEncoding {
    Hex,
    Binary,
}

impl DecodedAppend {
    /// 统一目录在入口将本地选择拆为基础 ID 和独立参数；官方选择保留原值。
    /// 续接子会话沿用已保存的结构化选择，不重新解释其来源。
    pub async fn resolve_model_selection(
        &mut self,
        registry: &TransportRegistry,
    ) -> Result<Option<crate::model::ModelSelection>> {
        let Some(agent::agent_client_message::Message::RunRequest(request)) =
            self.message.message.as_mut()
        else {
            return Ok(None);
        };
        let store = registry.store();
        let directory = ModelDirectory::configured(store, registry.plugins())
            .await
            .map_err(|error| Error::Protocol(error.to_string()))?;
        let saved = if request.subagent_type_name.is_some() {
            match request.conversation_id.as_deref() {
                Some(id) => {
                    store
                        .conversation_model_selection(&crate::model::ConversationId::new(id))
                        .await?
                }
                None => None,
            }
        } else {
            None
        };
        if request.requested_model.is_none() {
            if let Some(details) = &request.model_details {
                request.requested_model = Some(agent::RequestedModel {
                    model_id: details.model_id.clone(),
                    ..Default::default()
                });
            }
        }
        for model in request
            .requested_model
            .iter_mut()
            .filter(|_| saved.is_none())
            .chain(
                request
                    .subagent_model_overrides
                    .iter_mut()
                    .filter_map(|selection| match selection.selection.as_mut() {
                        Some(agent::subagent_model_override::Selection::Model(model)) => {
                            Some(model)
                        }
                        _ => None,
                    }),
            )
        {
            match directory.resolve(&model.model_id) {
                Resolution::Matched { id, parts } => {
                    if id != model.model_id {
                        model.model_id = id;
                        self.rewritten = true;
                    }
                    if let Some(parts) = parts {
                        self.rewritten = true;
                        // 子代理回程:已保存/烘焙的三个已知 id 定向覆盖,其余参数保留。
                        if request.subagent_type_name.is_some() {
                            model.parameters.retain(|parameter| {
                                !matches!(
                                    parameter.id.as_str(),
                                    "context" | "reasoning" | "effort" | "fast"
                                )
                            });
                        }
                        let mut parameters =
                            vec![("context", parts.context), ("fast", parts.fast.to_string())];
                        if let Some(effort) = parts.effort {
                            parameters.push(("reasoning", effort));
                        }
                        for (id, value) in parameters {
                            if !model
                                .parameters
                                .iter()
                                .any(|parameter| parameter.id == id && !parameter.value.is_empty())
                            {
                                model.parameters.push(
                                    agent::requested_model::ModelParameterValue {
                                        id: id.into(),
                                        value,
                                    },
                                );
                            }
                        }
                        drop_placeholder_duplicates(&mut model.parameters);
                    }
                }
                Resolution::Ambiguous { candidates } => {
                    return Err(Error::Config(format!(
                        "model '{}' is ambiguous; use one of the model IDs: {}",
                        model.model_id,
                        candidates.join(", ")
                    )))
                }
                Resolution::NotFound => {}
            }
        }
        if let Some(details) = request.model_details.as_mut() {
            if let Resolution::Matched { id, .. } = directory.resolve(&details.model_id) {
                if id != details.model_id {
                    details.model_id = id;
                    self.rewritten = true;
                }
            }
        }
        let selection = if let Some(saved) = saved {
            self.rewritten = true;
            if let Some(model) = request.requested_model.as_mut() {
                model.model_id = saved.id().to_owned();
                // 续接参数以已保存的选择为准,但只做定向覆盖:官方模型的参数原样
                // 透传给 Cursor,context/reasoning(含 effort 别名)/fast 之外的
                // 客户端参数(如 thinking)必须保留,不能按白名单过滤掉。
                model.parameters.retain(|parameter| {
                    !matches!(
                        parameter.id.as_str(),
                        "context" | "reasoning" | "effort" | "fast"
                    )
                });
                let parameters = saved.parameters();
                for (id, value) in [
                    ("context", parameters.context.clone()),
                    ("reasoning", parameters.reasoning.clone()),
                    ("fast", parameters.fast.map(|value| value.to_string())),
                ] {
                    if let Some(value) = value {
                        model
                            .parameters
                            .push(agent::requested_model::ModelParameterValue {
                                id: id.into(),
                                value,
                            });
                    }
                }
                drop_placeholder_duplicates(&mut model.parameters);
            }
            Some(saved)
        } else if let Some(model) = request.requested_model.as_ref() {
            let mut selection = crate::model::ModelSelection::resolve(&directory, &model.model_id)?;
            for parameter in &model.parameters {
                if parameter.value.trim().is_empty() {
                    continue;
                }
                match parameter.id.as_str() {
                    "context" => selection.parameters_mut().context = Some(parameter.value.clone()),
                    "reasoning" | "effort" => {
                        selection.parameters_mut().reasoning =
                            Some(parameter.value.trim().to_ascii_lowercase())
                    }
                    "fast" => {
                        selection.parameters_mut().fast =
                            Some(crate::cursor::compile::parse_bool(parameter)?)
                    }
                    _ => {}
                }
            }
            Some(selection)
        } else {
            None
        };
        Ok(selection)
    }

    pub fn model_id(&self) -> Option<&str> {
        let agent::agent_client_message::Message::RunRequest(request) =
            self.message.message.as_ref()?
        else {
            return None;
        };
        request
            .requested_model
            .as_ref()
            .map(|model| model.model_id.as_str())
            .filter(|model| !model.is_empty())
            .or_else(|| {
                request
                    .model_details
                    .as_ref()
                    .map(|model| model.model_id.as_str())
                    .filter(|model| !model.is_empty())
            })
    }

    pub fn conversation_id(&self) -> Option<&str> {
        let agent::agent_client_message::Message::RunRequest(request) =
            self.message.message.as_ref()?
        else {
            return None;
        };
        request.conversation_id.as_deref()
    }

    pub fn is_background_task_completion(&self) -> bool {
        let Some(agent::agent_client_message::Message::RunRequest(request)) =
            self.message.message.as_ref()
        else {
            return false;
        };
        matches!(
            request
                .action
                .as_ref()
                .and_then(|action| action.action.as_ref()),
            Some(agent::conversation_action::Action::BackgroundTaskCompletionAction(_))
        )
    }

    pub fn trace_metadata(&self) -> serde_json::Value {
        let Some(message) = self.message.message.as_ref() else {
            return serde_json::json!({
                "append_seqno": self.seqno,
                "message_type": "empty",
            });
        };
        let agent::agent_client_message::Message::RunRequest(request) = message else {
            return serde_json::json!({
                "append_seqno": self.seqno,
                "message_type": client_message_type(message),
            });
        };
        let (action_type, history_messages, history_images) = request
            .action
            .as_ref()
            .and_then(|action| action.action.as_ref())
            .map(|action| match action {
                agent::conversation_action::Action::UserMessageAction(action) => {
                    let history = action.conversation_history.as_ref();
                    (
                        "user_message",
                        history.map_or(0, |history| history.messages.len()),
                        history.map_or(0, history_image_count),
                    )
                }
                agent::conversation_action::Action::BackgroundTaskCompletionAction(_) => {
                    ("background_task_completion", 0, 0)
                }
                agent::conversation_action::Action::ExecutePlanAction(_) => ("execute_plan", 0, 0),
                agent::conversation_action::Action::SummarizeAction(_) => ("summarize", 0, 0),
                _ => ("other", 0, 0),
            })
            .unwrap_or(("none", 0, 0));
        let state = request.conversation_state.as_ref();
        serde_json::json!({
            "append_seqno": self.seqno,
            "message_type": "run_request",
            "conversation_id": request.conversation_id,
            "model_id": self.model_id(),
            "action_type": action_type,
            "conversation_history_messages": history_messages,
            "conversation_history_images": history_images,
            "root_message_count": state.map_or(0, |state| state.root_prompt_messages_json.len()),
            "turn_count": state.map_or(0, |state| state.turns.len()),
            "prefetched_blob_count": request.pre_fetched_blobs.len(),
        })
    }
}

fn client_message_type(message: &agent::agent_client_message::Message) -> &'static str {
    use agent::agent_client_message::Message;
    match message {
        Message::RunRequest(_) => "run_request",
        Message::ExecClientMessage(_) => "exec_client_message",
        Message::ExecClientControlMessage(_) => "exec_client_control_message",
        Message::KvClientMessage(_) => "kv_client_message",
        Message::ConversationAction(_) => "conversation_action",
        Message::InteractionResponse(_) => "interaction_response",
        Message::ClientHeartbeat(_) => "client_heartbeat",
        Message::PrewarmRequest(_) => "prewarm_request",
    }
}

fn history_image_count(history: &agent::ConversationHistory) -> usize {
    use agent::{
        conversation_history_message::Message,
        conversation_history_tool_result_content::Content as ToolContent,
        conversation_history_user_content::Content as UserContent,
    };
    history
        .messages
        .iter()
        .map(|message| match message.message.as_ref() {
            Some(Message::User(user)) => user
                .content
                .iter()
                .filter(|content| matches!(content.content, Some(UserContent::Image(_))))
                .count(),
            Some(Message::Tool(tool)) => tool
                .content
                .iter()
                .filter(|content| matches!(content.content, Some(ToolContent::Image(_))))
                .count(),
            _ => 0,
        })
        .sum()
}

/// 去掉同 id 空值占位的重复条目。Cursor 回程在未赋值轴上携带空值参数,
/// 续接/变体改写反复追加会造成同 id 空值条目累积(下游消费方跳过空值,
/// 无功能后果,仅协议噪声)。每个 id 的空值占位至多保留一条;非空值条目
/// 全部保留,优先级不变。
fn drop_placeholder_duplicates(parameters: &mut Vec<agent::requested_model::ModelParameterValue>) {
    let mut seen = std::collections::HashSet::new();
    parameters.retain(|parameter| {
        parameter.value.is_empty() && seen.insert(parameter.id.clone())
            || !parameter.value.is_empty()
    });
}

pub fn decode(request: &ai::BidiAppendRequest) -> Result<DecodedAppend> {
    let request_id = request
        .request_id
        .as_ref()
        .map(|id| id.request_id.as_str())
        .filter(|id| !id.is_empty())
        .ok_or_else(|| Error::Protocol("BidiAppend request_id is required".into()))?;
    // 双载荷歧义:客户端 statsig 开关切换的过渡窗口可能同时带 hex 与 binary。
    // 两种表示语义相同,无法判定以哪个为准,明确拒绝而不是静默取其一。
    let has_hex = !request.data.is_empty();
    let has_binary = !request.data_binary.is_empty();
    let encoding = match (has_hex, has_binary) {
        (true, true) => {
            return Err(Error::Protocol(
                "BidiAppend carries both data and data_binary; send exactly one".into(),
            ))
        }
        (true, false) => AppendEncoding::Hex,
        (false, true) => AppendEncoding::Binary,
        (false, false) => {
            return Err(Error::Protocol(
                "BidiAppend contains no AgentClientMessage".into(),
            ))
        }
    };
    let payload = match encoding {
        AppendEncoding::Hex => hex::decode(&request.data)
            .map_err(|error| Error::Protocol(format!("invalid BidiAppend hex: {error}")))?,
        AppendEncoding::Binary => request.data_binary.clone(),
    };
    Ok(DecodedAppend {
        request_id: request_id.into(),
        seqno: request.append_seqno,
        message: agent::AgentClientMessage::decode(payload.as_slice())?,
        rewritten: false,
        encoding,
    })
}

pub async fn append(
    registry: &TransportRegistry,
    request: DecodedAppend,
    parent: Option<TransportParent>,
) -> Result<ai::BidiAppendResponse> {
    let replace_closing = request.model_id().is_some();
    let handle = registry
        .get_or_create_for_append(&request.request_id, replace_closing)
        .await?;
    let _admission = handle.admit()?;
    if let Some(conversation_id) = request.conversation_id() {
        handle.set_conversation_id(conversation_id)?;
    }
    if let Some(parent) = parent {
        handle.set_parent(parent)?;
    }
    if matches!(
        request.message.message.as_ref(),
        Some(agent::agent_client_message::Message::ClientHeartbeat(_))
    ) {
        handle.emit(&events::heartbeat())?;
    }
    handle
        .command(TransportCommand::Append {
            seqno: request.seqno,
            message: Box::new(request.message),
        })
        .await?;
    Ok(ai::BidiAppendResponse {})
}

#[cfg(test)]
mod selection_tests {
    use super::*;
    use crate::provider::{FinishReason, ModelEvent, Provider, ProviderStream};
    use std::sync::Arc;
    use tokio_util::sync::CancellationToken;

    struct NoopProvider;
    impl Provider for NoopProvider {
        fn stream(
            &self,
            _invocation: crate::model::ModelInvocation,
            _cancellation: CancellationToken,
        ) -> ProviderStream {
            Box::pin(async_stream::try_stream! {
                yield ModelEvent::Done(FinishReason::Stop);
            })
        }
    }

    fn registry(store: crate::store::Store) -> TransportRegistry {
        TransportRegistry::new(
            store,
            Arc::new(NoopProvider),
            crate::cursor::prompting::PromptCompiler::new(
                crate::cursor::prompting::PromptAssets::embedded().unwrap(),
            ),
        )
    }

    #[tokio::test]
    async fn resolves_all_explicit_selections_but_preserves_inheritance_and_parameters() {
        let directory = tempfile::tempdir().unwrap();
        let store = crate::store::Store::connect(&format!(
            "sqlite://{}",
            directory.path().join("test.db").display()
        ))
        .await
        .unwrap();
        let model = store.create_model(&serde_json::from_value(serde_json::json!({
            "display_name": "BYOK model", "type": "openai", "base_url": "https://example.invalid",
            "api_key": "test", "tooltip_data": "test", "model_id": "provider-model"
        })).unwrap()).await.unwrap();
        let selection = agent::RequestedModel {
            model_id: "provider-model".into(),
            parameters: vec![agent::requested_model::ModelParameterValue {
                id: "effort".into(),
                value: "high".into(),
            }],
            ..Default::default()
        };
        let mut decoded = DecodedAppend {
            request_id: "selections".into(),
            seqno: 0,
            rewritten: false,
            encoding: AppendEncoding::Hex,
            message: agent::AgentClientMessage {
                message: Some(agent::agent_client_message::Message::RunRequest(
                    agent::AgentRunRequest {
                        requested_model: Some(selection.clone()),
                        model_details: Some(agent::ModelDetails {
                            model_id: "BYOK Model".into(),
                            ..Default::default()
                        }),
                        subagent_model_overrides: vec![
                            agent::SubagentModelOverride {
                                subagent_type: "generalPurpose".into(),
                                selection: Some(agent::subagent_model_override::Selection::Model(
                                    selection.clone(),
                                )),
                            },
                            agent::SubagentModelOverride {
                                subagent_type: "explore".into(),
                                selection: Some(
                                    agent::subagent_model_override::Selection::Inherit(true),
                                ),
                            },
                        ],
                        ..Default::default()
                    },
                )),
            },
        };
        decoded
            .resolve_model_selection(&registry(store))
            .await
            .unwrap();
        let Some(agent::agent_client_message::Message::RunRequest(run)) = decoded.message.message
        else {
            unreachable!()
        };
        let requested = run.requested_model.unwrap();
        assert_eq!(requested.model_id, model.model_hash);
        assert_eq!(requested.parameters, selection.parameters);
        assert_eq!(run.model_details.unwrap().model_id, model.model_hash);
        let Some(agent::subagent_model_override::Selection::Model(child)) =
            &run.subagent_model_overrides[0].selection
        else {
            unreachable!()
        };
        assert_eq!(child.model_id, model.model_hash);
        assert_eq!(child.parameters, selection.parameters);
        assert_eq!(
            run.subagent_model_overrides[1].selection,
            Some(agent::subagent_model_override::Selection::Inherit(true))
        );
    }

    #[tokio::test]
    async fn saved_subagent_selection_controls_routing_without_reinterpreting_official_names() {
        let store = crate::store::Store::connect("sqlite::memory:")
            .await
            .unwrap();
        let model = store.create_model(&serde_json::from_value(serde_json::json!({
            "display_name":"Hosted name", "type":"openai", "base_url":"https://example.invalid", "api_key":"test", "tooltip_data":"test", "model_id":"upstream-name"
        })).unwrap()).await.unwrap();
        let id = crate::model::ConversationId::new("saved-child");
        store.ensure_conversation(&id).await.unwrap();
        let registry = registry(store.clone());
        for saved in [
            crate::model::ModelSelection::Official {
                model: "Hosted name".into(),
                parameters: Default::default(),
            },
            crate::model::ModelSelection::Local {
                id: model.model_hash.clone(),
                parameters: crate::model::SelectionParameters {
                    context: Some("1m".into()),
                    reasoning: Some("low".into()),
                    fast: Some(true),
                },
            },
        ] {
            store
                .set_conversation_model_selection(&id, Some(&saved))
                .await
                .unwrap();
            let mut decoded = DecodedAppend {
                request_id: "resumed".into(),
                seqno: 0,
                rewritten: false,
                encoding: AppendEncoding::Hex,
                message: agent::AgentClientMessage {
                    message: Some(agent::agent_client_message::Message::RunRequest(
                        agent::AgentRunRequest {
                            conversation_id: Some(id.to_string()),
                            subagent_type_name: Some("generalPurpose".into()),
                            requested_model: Some(agent::RequestedModel {
                                model_id: "stale-client-value".into(),
                                parameters: vec![
                                    agent::requested_model::ModelParameterValue {
                                        id: "thinking".into(),
                                        value: "true".into(),
                                    },
                                    agent::requested_model::ModelParameterValue {
                                        id: "effort".into(),
                                        value: "stale".into(),
                                    },
                                ],
                                ..Default::default()
                            }),
                            ..Default::default()
                        },
                    )),
                },
            };
            assert_eq!(
                decoded.resolve_model_selection(&registry).await.unwrap(),
                Some(saved.clone())
            );
            assert_eq!(decoded.model_id(), Some(saved.id()));
            // 续接按保存的选择定向覆盖三个已知 id;未知参数(如 thinking)
            // 原样保留,官方回程不做白名单过滤;effort 别名被 reasoning 覆盖掉。
            assert!(decoded.rewritten);
            let Some(agent::agent_client_message::Message::RunRequest(run)) =
                decoded.message.message.as_ref()
            else {
                unreachable!()
            };
            let parameters = &run.requested_model.as_ref().unwrap().parameters;
            assert!(
                parameters
                    .iter()
                    .any(|parameter| parameter.id == "thinking" && parameter.value == "true"),
                "unknown parameters must survive continuation: {parameters:?}"
            );
            assert!(
                !parameters.iter().any(|parameter| parameter.id == "effort"),
                "the effort alias is replaced by reasoning: {parameters:?}"
            );
        }
    }

    #[tokio::test]
    async fn selection_parameters_normalize_reasoning_and_reject_invalid_fast() {
        let store = crate::store::Store::connect("sqlite::memory:")
            .await
            .unwrap();
        let registry = registry(store);
        for (fast, valid) in [("true", true), ("false", true), ("invalid", false)] {
            let mut decoded = DecodedAppend {
                request_id: "parameters".into(),
                seqno: 0,
                rewritten: false,
                encoding: AppendEncoding::Hex,
                message: agent::AgentClientMessage {
                    message: Some(agent::agent_client_message::Message::RunRequest(
                        agent::AgentRunRequest {
                            requested_model: Some(agent::RequestedModel {
                                model_id: "official-model".into(),
                                parameters: [("reasoning", " HIGH "), ("fast", fast)]
                                    .into_iter()
                                    .map(|(id, value)| {
                                        agent::requested_model::ModelParameterValue {
                                            id: id.into(),
                                            value: value.into(),
                                        }
                                    })
                                    .collect(),
                                ..Default::default()
                            }),
                            ..Default::default()
                        },
                    )),
                },
            };
            let result = decoded.resolve_model_selection(&registry).await;
            assert!(
                !decoded.rewritten,
                "an unmatched official model is not rewritten"
            );
            if valid {
                let selection = result.unwrap().unwrap();
                assert_eq!(selection.parameters().reasoning.as_deref(), Some("high"));
                assert_eq!(selection.parameters().fast, Some(fast == "true"));
            } else {
                assert!(result
                    .unwrap_err()
                    .to_string()
                    .contains("invalid Cursor boolean"));
            }
        }
    }

    #[tokio::test]
    async fn directory_hits_normalize_to_base_ids() {
        let directory = tempfile::tempdir().unwrap();
        let store = crate::store::Store::connect(&format!(
            "sqlite://{}",
            directory.path().join("test.db").display()
        ))
        .await
        .unwrap();
        let model = store.create_model(&serde_json::from_value(serde_json::json!({
            "display_name": "BYOK model", "type": "openai", "base_url": "https://example.invalid",
            "api_key": "test", "tooltip_data": "test", "model_id": "provider-model"
        })).unwrap()).await.unwrap();
        let mut decoded = DecodedAppend {
            request_id: "name-hit".into(),
            seqno: 0,
            rewritten: false,
            encoding: AppendEncoding::Hex,
            message: agent::AgentClientMessage {
                message: Some(agent::agent_client_message::Message::RunRequest(
                    agent::AgentRunRequest {
                        requested_model: Some(agent::RequestedModel {
                            model_id: "BYOK Model".into(),
                            ..Default::default()
                        }),
                        model_details: Some(agent::ModelDetails {
                            model_id: format!("{}-1m-low", model.model_hash),
                            ..Default::default()
                        }),
                        ..Default::default()
                    },
                )),
            },
        };
        decoded
            .resolve_model_selection(&registry(store))
            .await
            .unwrap();
        let Some(agent::agent_client_message::Message::RunRequest(run)) = decoded.message.message
        else {
            unreachable!()
        };
        // 名称与变体输入均在 API 边界拆为基础 ID 和参数。
        assert!(decoded.rewritten);
        assert_eq!(run.requested_model.unwrap().model_id, model.model_hash);
        assert_eq!(run.model_details.unwrap().model_id, model.model_hash);
    }
}
