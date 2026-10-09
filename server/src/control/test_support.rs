//! Shared test fixtures for the control API modules.
#![cfg(test)]

use std::sync::{Arc, Mutex};

use tokio_util::sync::CancellationToken;

use crate::{
    model::{ModelConfig, ModelConfigInput, ModelInvocation, ModelType},
    plugin::{PluginRegistry, PluginRuntime},
    provider::{FinishReason, ModelEvent, Provider, ProviderStream},
    store::Store,
};

use super::{AccessToken, AccessTokenSource, ControlService};

pub(super) struct TestProvider {
    pub(super) invocation: Arc<Mutex<Option<ModelInvocation>>>,
}

pub(super) struct CancellationProvider {
    pub(super) started: Arc<tokio::sync::Notify>,
}

impl Provider for TestProvider {
    fn stream(
        &self,
        invocation: ModelInvocation,
        _cancellation: CancellationToken,
    ) -> ProviderStream {
        *self.invocation.lock().unwrap() = Some(invocation);
        Box::pin(futures_util::stream::iter([
            Ok(ModelEvent::Start {
                model_call_id: "test-call".into(),
            }),
            Ok(ModelEvent::TextStart),
            Ok(ModelEvent::TextDelta("OK".into())),
            Ok(ModelEvent::TextEnd),
            Ok(ModelEvent::Usage(crate::model::Usage {
                output_tokens: Some(2),
                ..Default::default()
            })),
            Ok(ModelEvent::Done(FinishReason::Stop)),
        ]))
    }
}

impl Provider for CancellationProvider {
    fn stream(
        &self,
        _invocation: ModelInvocation,
        cancellation: CancellationToken,
    ) -> ProviderStream {
        let started = self.started.clone();
        Box::pin(async_stream::try_stream! {
            started.notify_one();
            cancellation.cancelled().await;
            if false { yield ModelEvent::TextStart; }
        })
    }
}

pub(super) async fn create_test_model(store: &Store) -> ModelConfig {
    store
        .create_model(&ModelConfigInput {
            model_id: "reasoning-model".into(),
            display_name: "Reasoning Model".into(),
            group_name: None,
            model_type: ModelType::OpenAi,
            base_url: "https://example.com/v1/responses".into(),
            use_full_url: true,
            api_key: "secret".into(),
            tooltip_data: "Reasoning Model".into(),
            sort_order: 0,
            default_context: None,
            // 轴包含默认档位:保存入口要求默认落在最终轴上(M-1 修复后不再
            // 接受轴外默认),connectivity 测试继续断言默认 "medium" 生效。
            default_effort: Some("medium".into()),
            effort_options: vec!["low".into(), "medium".into(), "high".into()],
            context_options: vec!["200k".into(), "1m".into()],
            openai_endpoint: "/v1/responses".into(),
            openai_extra_params_enabled: false,
            openai_extra_params: serde_json::json!({}),
            custom_headers_enabled: false,
            custom_headers: serde_json::json!({}),
            anthropic_extra_params_enabled: false,
            anthropic_extra_params: serde_json::json!({}),
            context_window_tokens: None,
            max_completion_tokens: None,
            anthropic_max_tokens: None,
            thinking_budget_tokens: None,
        })
        .await
        .unwrap()
}

pub(super) fn service_for(store: crate::store::Store) -> ControlService {
    let plugin_runtime = PluginRuntime::managed().unwrap();
    let plugins =
        PluginRegistry::managed(store.clone(), plugin_runtime.clone(), "test".into()).unwrap();
    let clients = crate::network::NetworkClients::new(store.clone());
    ControlService::new(
        store,
        Arc::new(TestProvider {
            invocation: Arc::new(Mutex::new(None)),
        }),
        plugin_runtime,
        plugins,
        clients,
        AccessToken::new("test".into(), AccessTokenSource::Generated),
    )
    .unwrap()
}

pub(super) async fn model_with_headers(store: &crate::store::Store) -> crate::model::ModelConfig {
    let input = ModelConfigInput {
        model_id: "header-model".into(),
        display_name: "Header Model".into(),
        group_name: None,
        model_type: ModelType::OpenAi,
        base_url: "https://example.com/v1/responses".into(),
        use_full_url: true,
        api_key: "secret".into(),
        tooltip_data: "Header Model".into(),
        sort_order: 0,
        default_context: None,
        default_effort: None,
        effort_options: Vec::new(),
        context_options: Vec::new(),
        openai_endpoint: "/v1/responses".into(),
        openai_extra_params_enabled: false,
        openai_extra_params: serde_json::json!({}),
        custom_headers_enabled: true,
        custom_headers: serde_json::json!({
            "Authorization": "Bearer token",
            "X-Tenant": "tenant-a"
        }),
        anthropic_extra_params_enabled: false,
        anthropic_extra_params: serde_json::json!({}),
        context_window_tokens: None,
        max_completion_tokens: None,
        anthropic_max_tokens: None,
        thinking_budget_tokens: None,
    };
    store.create_model(&input).await.unwrap()
}
