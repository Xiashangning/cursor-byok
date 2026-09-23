//! Implements control API routing and shared state.
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex},
    time::Instant,
};

use base64::{engine::general_purpose::STANDARD, Engine};
use futures_util::StreamExt;
use reqwest::header::{HeaderName, HeaderValue};
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::{
    control::auth::{AccessToken, AccessTokenSource},
    local_app::CursorHarness,
    model::{
        ContentPart, CursorRunTraceArtifact, CursorRunTraceSummary, LlmCallRequest,
        LlmCallResponseChunk, LlmCallSummary, ModelConfig, ModelConfigInput, ModelInvocation,
        ModelRequest, ModelSpec, ModelType, Overview, ProjectedContent, ProjectedMessage,
        PromptSpec, ProviderType, Role,
    },
    plugin::{PluginDescriptor, PluginRegistry, PluginRuntime, PluginRuntimeStatus},
    provider::{is_valid_response_event, ModelEvent, Provider},
    store::{
        CommitSettings, DesktopSettings, PluginModelOverride, PortSettings, ProxySettings,
        ProxySettingsInput, StatisticsStorage, Store, TabSettings,
    },
    Error, Result,
};

#[derive(Clone)]
pub struct ControlService {
    store: Store,
    cursor_harness: CursorHarness,
    provider: Arc<dyn Provider>,
    plugin_runtime: PluginRuntime,
    plugins: PluginRegistry,
    clients: crate::network::NetworkClients,
    access_token: AccessToken,
    model_tests: Arc<Mutex<BTreeMap<String, CancellationToken>>>,
}

#[derive(Clone, Debug, Serialize)]
pub struct DiscoveredModels {
    pub models: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct AccessTokenView {
    pub token: String,
    pub source: AccessTokenSource,
}

#[derive(Clone, Debug, Deserialize)]
pub struct ModelDiscoveryInput {
    #[serde(rename = "type")]
    pub model_type: ModelType,
    pub base_url: String,
    pub api_key: String,
    /// 编辑已有模型时带上哈希:编辑器往返的是脱敏配置(REDACTED_SECRET 占位符),
    /// 占位符密钥/敏感头由服务端回填。
    #[serde(default)]
    pub model_hash: Option<String>,
    #[serde(default)]
    pub custom_headers_enabled: bool,
    #[serde(default = "empty_json_object")]
    pub custom_headers: serde_json::Value,
}

fn empty_json_object() -> serde_json::Value {
    serde_json::json!({})
}

fn empty_json_object_ref() -> &'static serde_json::Value {
    static EMPTY: std::sync::OnceLock<serde_json::Value> = std::sync::OnceLock::new();
    EMPTY.get_or_init(empty_json_object)
}

#[derive(Clone, Debug, Serialize)]
pub struct ModelConnectivityResult {
    pub duration_ms: u64,
    pub first_valid_response_ms: Option<u64>,
    pub output_tokens: u64,
    pub tokens_per_second: f64,
    pub tokens_estimated: bool,
    pub output: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct CallDetail {
    pub call: CallSummary,
    pub request: Option<LlmCallRequest>,
    pub response_chunks: Vec<LlmCallResponseChunk>,
    pub cursor_trace: Option<CursorTraceDetail>,
}

#[derive(Clone, Debug, Serialize)]
pub struct CallSummary {
    #[serde(flatten)]
    pub call: LlmCallSummary,
    pub call_kind: &'static str,
    pub route: &'static str,
}

#[derive(Clone, Debug, Serialize)]
pub struct CursorTraceDetail {
    pub trace: CursorRunTraceSummary,
    pub artifacts: Vec<CursorTraceArtifactDetail>,
}

#[derive(Clone, Debug, Serialize)]
pub struct CursorTraceArtifactDetail {
    pub seq: i64,
    pub artifact_type: String,
    pub source: String,
    pub metadata: serde_json::Value,
    pub created_at_ms: i64,
    pub byte_count: usize,
    pub encoding: &'static str,
    pub data: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
pub struct ObservabilitySettings {
    pub detailed: bool,
}

impl ControlService {
    pub fn new(
        store: Store,
        provider: Arc<dyn Provider>,
        plugin_runtime: PluginRuntime,
        plugins: PluginRegistry,
        clients: crate::network::NetworkClients,
        access_token: AccessToken,
    ) -> Result<Self> {
        Ok(Self {
            cursor_harness: CursorHarness::new(store.clone())?,
            store,
            provider,
            plugin_runtime,
            plugins,
            clients,
            access_token,
            model_tests: Arc::new(Mutex::new(BTreeMap::new())),
        })
    }

    pub fn access_token(&self) -> &AccessToken {
        &self.access_token
    }

    pub async fn regenerate_access_token(&self) -> Result<AccessTokenView> {
        let token = self.access_token.regenerate(&self.store).await?;
        Ok(AccessTokenView {
            token,
            source: self.access_token.source(),
        })
    }

    pub fn access_token_view(&self) -> AccessTokenView {
        AccessTokenView {
            token: self.access_token.current(),
            source: self.access_token.source(),
        }
    }

    pub fn cursor_harness(&self) -> &CursorHarness {
        &self.cursor_harness
    }

    pub async fn plugins(&self) -> Vec<PluginDescriptor> {
        self.plugins.plugins().await
    }

    pub async fn plugin_oauth_begin(
        &self,
        plugin_id: &str,
        resource_type: &str,
        method_id: &str,
    ) -> Result<crate::plugin::OAuthBeginResponse> {
        self.plugins
            .oauth_begin(plugin_id, resource_type, method_id)
            .await
    }

    pub async fn plugin_oauth_poll(
        &self,
        session_id: &str,
    ) -> Result<crate::plugin::OAuthPollResponse> {
        self.plugins.oauth_poll(session_id).await
    }

    pub async fn plugin_import(
        &self,
        plugin_id: &str,
        resource_type: &str,
        files: serde_json::Value,
    ) -> Result<crate::plugin::ImportResponse> {
        self.plugins
            .import_resources(plugin_id, resource_type, files)
            .await
    }

    pub async fn plugin_export_resources(
        &self,
        plugin_id: &str,
        resource_type: &str,
    ) -> Result<serde_json::Value> {
        self.plugins
            .export_resources(plugin_id, resource_type)
            .await
    }

    pub async fn plugin_refresh_resource(
        &self,
        plugin_id: &str,
        resource_type: &str,
        resource_id: &str,
    ) -> Result<()> {
        self.plugins
            .refresh_resource(plugin_id, resource_type, resource_id)
            .await
    }

    pub async fn plugin_resource_action(
        &self,
        plugin_id: &str,
        resource_type: &str,
        resource_id: &str,
        action_id: &str,
        input: serde_json::Value,
    ) -> Result<serde_json::Value> {
        self.plugins
            .resource_action(plugin_id, resource_type, resource_id, action_id, input)
            .await
    }

    pub async fn plugin_delete_resource(
        &self,
        plugin_id: &str,
        resource_type: &str,
        resource_id: &str,
    ) -> Result<()> {
        self.plugins
            .delete_resource(plugin_id, resource_type, resource_id)
            .await
    }

    pub async fn plugin_sync_models(&self, plugin_id: &str, provider_id: &str) -> Result<usize> {
        self.plugins.sync_models(plugin_id, provider_id).await
    }

    pub async fn plugin_set_model_enabled(
        &self,
        plugin_id: &str,
        provider_id: &str,
        model_id: &str,
        enabled: bool,
    ) -> Result<()> {
        self.plugins
            .set_model_enabled(plugin_id, provider_id, model_id, enabled)
            .await
    }

    pub async fn remove_plugin_configuration(&self, plugin_id: &str) -> Result<()> {
        self.plugins.remove(plugin_id).await
    }

    pub fn plugin_runtime_status(&self) -> PluginRuntimeStatus {
        self.plugin_runtime.status()
    }

    pub fn initialize_plugin_runtime(&self) -> PluginRuntimeStatus {
        self.plugin_runtime.initialize(self.store.clone())
    }

    pub fn cancel_plugin_runtime_initialization(&self) -> PluginRuntimeStatus {
        self.plugin_runtime.cancel_initialization()
    }

    pub async fn disabled_plugin_models(&self) -> Result<Vec<String>> {
        let mut list: Vec<_> = self
            .store
            .disabled_plugin_models()
            .await?
            .into_iter()
            .collect();
        list.sort();
        Ok(list)
    }

    pub async fn set_disabled_plugin_models(&self, model_ids: Vec<String>) -> Result<()> {
        let set = model_ids.into_iter().collect();
        self.store.set_disabled_plugin_models(&set).await
    }

    pub async fn disabled_plugin_accounts(&self) -> Result<Vec<String>> {
        let mut list: Vec<_> = self
            .store
            .disabled_plugin_accounts()
            .await?
            .into_iter()
            .collect();
        list.sort();
        Ok(list)
    }

    pub async fn set_disabled_plugin_accounts(&self, account_ids: Vec<String>) -> Result<()> {
        let set = account_ids.into_iter().collect();
        self.store.set_disabled_plugin_accounts(&set).await
    }

    pub async fn set_plugin_model_override(
        &self,
        id: String,
        over: PluginModelOverride,
    ) -> Result<()> {
        self.store.set_plugin_model_override(&id, over).await
    }

    pub async fn models(&self) -> Result<Vec<ModelConfig>> {
        self.store.models().await
    }

    pub async fn overview(
        &self,
        start_ms: Option<i64>,
        end_ms: Option<i64>,
        model_hashes: Option<&str>,
        bucket_ms: Option<i64>,
    ) -> Result<Overview> {
        self.store
            .overview(start_ms, end_ms, model_hashes, bucket_ms)
            .await
    }

    pub async fn create_models(&self, models: &[ModelConfigInput]) -> Result<Vec<ModelConfig>> {
        self.store.create_models(models).await
    }

    pub async fn reorder_models(&self, model_hashes: &[String]) -> Result<Vec<ModelConfig>> {
        self.store.reorder_models(model_hashes).await
    }

    pub async fn delete_model(&self, model_hash: &str) -> Result<()> {
        self.store.delete_model(model_hash).await
    }

    pub async fn update_model(
        &self,
        model_hash: &str,
        input: &ModelConfigInput,
    ) -> Result<ModelConfig> {
        let mut input = input.clone();
        let existing = self
            .store
            .model(model_hash)
            .await?
            .ok_or_else(|| Error::RunNotFound(format!("model {model_hash}")))?;
        if input.api_key == crate::model::REDACTED_SECRET {
            input.api_key = existing.api_key;
        }
        restore_redacted_headers(&mut input.custom_headers, &existing.custom_headers);
        self.store.update_model(model_hash, &input).await
    }

    pub async fn duplicate_model(
        &self,
        model_hash: &str,
        display_name: String,
        sort_order: i64,
    ) -> Result<ModelConfig> {
        let existing = self
            .store
            .model(model_hash)
            .await?
            .ok_or_else(|| Error::RunNotFound(format!("model {model_hash}")))?;
        let mut input = existing.into_input();
        input.display_name = display_name;
        input.sort_order = sort_order;
        self.store.create_model(&input).await
    }

    pub async fn test_model(
        &self,
        model_hash: &str,
        test_id: &str,
    ) -> Result<ModelConnectivityResult> {
        let cancellation = CancellationToken::new();
        let cancellation = {
            let mut tests = self
                .model_tests
                .lock()
                .expect("model test registry mutex poisoned");
            tests
                .entry(test_id.to_owned())
                .or_insert_with(|| cancellation.clone())
                .clone()
        };
        let result = self.run_model_test(model_hash, cancellation).await;
        self.model_tests
            .lock()
            .expect("model test registry mutex poisoned")
            .remove(test_id);
        result
    }

    pub fn cancel_model_test(&self, test_id: &str) {
        let cancellation = {
            let mut tests = self
                .model_tests
                .lock()
                .expect("model test registry mutex poisoned");
            tests.entry(test_id.to_owned()).or_default().clone()
        };
        cancellation.cancel();
    }

    async fn run_model_test(
        &self,
        model_hash: &str,
        cancellation: CancellationToken,
    ) -> Result<ModelConnectivityResult> {
        const TEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(45);
        const TEST_PROMPT: &str = "Output the numbers 1 through 120 separated by a single space. No commas, no newlines, no explanation.";

        let mut model = ModelSpec::new(model_hash);
        if model_hash.starts_with(crate::plugin::ADAPTER_ID_PREFIX) {
            let descriptor = self.plugins.model_descriptor(model_hash).await?;
            model.display_name = Some(descriptor.display_name);
            model.max_output_tokens = Some(descriptor.max_output_tokens.unwrap_or(65_536));
        } else {
            let configured = self
                .store
                .model(model_hash)
                .await?
                .ok_or_else(|| Error::RunNotFound(format!("model {model_hash}")))?;
            configured.configure(&mut model);
            model.max_output_tokens = Some(configured.max_output_tokens().unwrap_or(65_536));
        }
        let call_id = format!("model-test-{}", uuid::Uuid::new_v4());
        let invocation = ModelInvocation {
            call_id: call_id.clone(),
            run_id: call_id.clone(),
            conversation_id: call_id.clone(),
            provider_call_index: 0,
            request: ModelRequest {
                prompt: PromptSpec {
                    instructions: String::new(),
                    tools: Vec::new(),
                },
                model,
                history: vec![ProjectedMessage {
                    message_id: "connectivity-test".into(),
                    role: Role::User,
                    content: ProjectedContent::Parts(vec![ContentPart::Text {
                        text: TEST_PROMPT.into(),
                    }]),
                }],
            },
        };
        let started = Instant::now();
        let mut first_valid_response_at = None;
        let mut output_tokens = None;
        let mut output = String::new();
        let stream = self.provider.stream(invocation, cancellation.clone());
        let completed = tokio::time::timeout(TEST_TIMEOUT, async {
            futures_util::pin_mut!(stream);
            let mut finished = false;
            while let Some(event) = stream.next().await {
                let event = event?;
                if first_valid_response_at.is_none() && is_valid_response_event(&event) {
                    first_valid_response_at = Some(Instant::now());
                }
                match event {
                    ModelEvent::TextDelta(delta) => {
                        output.push_str(&delta);
                    }
                    ModelEvent::Usage(usage) => {
                        if let Some(tokens) = usage.output_tokens.filter(|tokens| *tokens > 0) {
                            output_tokens = Some(
                                output_tokens.map_or(tokens, |current: u64| current.max(tokens)),
                            );
                        }
                    }
                    ModelEvent::Done(_) => finished = true,
                    _ => {}
                }
            }
            if cancellation.is_cancelled() {
                return Err(Error::Cancelled);
            }
            if !finished {
                return Err(Error::Protocol(
                    "provider stream ended without Done during connectivity test".into(),
                ));
            }
            Ok(())
        })
        .await;
        match completed {
            Ok(result) => result?,
            Err(_) => {
                cancellation.cancel();
                self.store
                    .finish_llm_call(
                        &call_id,
                        "error",
                        None,
                        started.elapsed().as_millis().min(i64::MAX as u128) as i64,
                        Some("timeout"),
                        Some("model connectivity test timed out after 45 seconds"),
                    )
                    .await?;
                return Err(Error::Provider(
                    "model connectivity test timed out after 45 seconds".into(),
                ));
            }
        }
        let elapsed = started.elapsed();
        let output = output.trim().to_string();
        if first_valid_response_at.is_none() {
            return Err(Error::Provider(
                "model connectivity test received no valid response".into(),
            ));
        }
        let tokens_estimated = output_tokens.is_none();
        let output_tokens = output_tokens.unwrap_or_else(|| estimate_output_tokens(&output));
        Ok(ModelConnectivityResult {
            duration_ms: elapsed.as_millis().min(u128::from(u64::MAX)) as u64,
            first_valid_response_ms: first_valid_response_at.map(|first| {
                first
                    .duration_since(started)
                    .as_millis()
                    .min(u128::from(u64::MAX)) as u64
            }),
            output_tokens,
            tokens_per_second: if elapsed.is_zero() {
                0.0
            } else {
                output_tokens as f64 / elapsed.as_secs_f64()
            },
            tokens_estimated,
            output,
        })
    }

    pub async fn discover_models(&self, input: &ModelDiscoveryInput) -> Result<DiscoveredModels> {
        let (api_key, custom_headers) = self.discovery_credentials(input).await?;
        let client = self.clients.default_client().await?;
        let base_url = crate::model::normalize_request_url(&input.base_url)?;
        discover_models_from_endpoint(
            &client,
            match input.model_type {
                ModelType::OpenAi => ProviderType::OpenAiResponses,
                ModelType::Anthropic => ProviderType::Anthropic,
            },
            &base_url,
            &api_key,
            if input.custom_headers_enabled {
                &custom_headers
            } else {
                empty_json_object_ref()
            },
        )
        .await
    }

    /// 编辑已有模型时,编辑器里往返的是脱敏配置(REDACTED_SECRET 占位符),
    /// 发现模型前从存储回填;占位符语义与更新模型一致(空串即「清除」,不回填)。
    /// 一旦回填了存储的密钥,目标 URL 必须仍是该模型自己配置的地址,
    /// 否则等于把密钥发给调用方指定的任意服务器。
    async fn discovery_credentials(
        &self,
        input: &ModelDiscoveryInput,
    ) -> Result<(String, serde_json::Value)> {
        let Some(model_hash) = input.model_hash.as_deref() else {
            return Ok((input.api_key.clone(), input.custom_headers.clone()));
        };
        let stored = self
            .store
            .model(model_hash)
            .await?
            .ok_or_else(|| Error::RunNotFound(format!("model {model_hash}")))?;
        let redacted_key = input.api_key == crate::model::REDACTED_SECRET;
        let api_key = if redacted_key {
            stored.api_key.clone()
        } else {
            input.api_key.clone()
        };
        let mut custom_headers = input.custom_headers.clone();
        let backfilled_headers =
            restore_redacted_headers(&mut custom_headers, &stored.custom_headers);
        if (redacted_key && !api_key.is_empty()) || backfilled_headers > 0 {
            let requested = crate::model::normalize_request_url(&input.base_url)?;
            let configured = crate::model::normalize_request_url(&stored.base_url)?;
            if requested.trim_end_matches('/') != configured.trim_end_matches('/') {
                return Err(Error::Config(
                    "model discovery with stored credentials requires the model's configured base URL; re-enter the API key to discover from a different URL".into(),
                ));
            }
        }
        Ok((api_key, custom_headers))
    }

    pub async fn calls(&self, limit: i64) -> Result<Vec<CallSummary>> {
        let mut calls = self
            .store
            .llm_calls(limit)
            .await?
            .into_iter()
            .map(|call| CallSummary {
                call,
                call_kind: "provider_llm",
                route: "local_byok",
            })
            .collect::<Vec<_>>();
        calls.extend(
            self.store
                .official_cursor_traces(limit)
                .await?
                .into_iter()
                .map(official_call),
        );
        calls.sort_by_key(|call| std::cmp::Reverse(call.call.created_at_ms));
        calls.truncate(limit.clamp(1, 500) as usize);
        Ok(calls)
    }

    pub async fn call(&self, call_id: &str) -> Result<CallDetail> {
        if let Some(call) = self.store.llm_call(call_id).await? {
            let cursor_trace = self.cursor_trace_detail(&call.run_id).await?;
            return Ok(CallDetail {
                request: self.store.llm_call_request(call_id).await?,
                response_chunks: self.store.llm_call_chunks(call_id).await?,
                call: CallSummary {
                    call,
                    call_kind: "provider_llm",
                    route: "local_byok",
                },
                cursor_trace,
            });
        }
        let request_id = call_id.strip_prefix("cursor:").unwrap_or(call_id);
        let trace = self
            .store
            .cursor_trace(request_id)
            .await?
            .filter(|trace| trace.route == "cursor_official")
            .ok_or_else(|| Error::RunNotFound(format!("call {call_id}")))?;
        Ok(CallDetail {
            call: official_call(trace.clone()),
            request: None,
            response_chunks: Vec::new(),
            cursor_trace: Some(self.cursor_trace_detail_from(trace).await?),
        })
    }

    async fn cursor_trace_detail(&self, request_id: &str) -> Result<Option<CursorTraceDetail>> {
        let Some(trace) = self.store.cursor_trace(request_id).await? else {
            return Ok(None);
        };
        Ok(Some(self.cursor_trace_detail_from(trace).await?))
    }

    async fn cursor_trace_detail_from(
        &self,
        trace: CursorRunTraceSummary,
    ) -> Result<CursorTraceDetail> {
        let artifacts = self
            .store
            .cursor_trace_artifacts(&trace.request_id)
            .await?
            .into_iter()
            .map(cursor_artifact)
            .collect();
        Ok(CursorTraceDetail { trace, artifacts })
    }

    pub async fn observability(&self) -> Result<ObservabilitySettings> {
        Ok(ObservabilitySettings {
            detailed: self.store.detailed_logging().await?,
        })
    }

    pub async fn set_observability(
        &self,
        settings: ObservabilitySettings,
    ) -> Result<ObservabilitySettings> {
        self.store.set_detailed_logging(settings.detailed).await?;
        Ok(settings)
    }

    pub async fn ports(&self) -> Result<PortSettings> {
        self.store.port_settings().await
    }

    pub async fn set_ports(&self, settings: PortSettings) -> Result<PortSettings> {
        self.store.set_port_settings(settings).await?;
        Ok(settings)
    }

    pub async fn statistics_storage(&self) -> Result<StatisticsStorage> {
        self.store.statistics_storage().await
    }

    pub async fn clear_statistics_storage(&self) -> Result<StatisticsStorage> {
        self.store.clear_statistics_storage().await
    }

    pub async fn clear_all_statistics_storage(&self) -> Result<StatisticsStorage> {
        self.store.clear_all_statistics_storage().await
    }

    pub async fn proxy_settings(&self) -> Result<ProxySettings> {
        self.store.proxy_settings().await
    }

    pub async fn set_proxy_settings(&self, settings: ProxySettingsInput) -> Result<ProxySettings> {
        if settings.mode.is_custom() {
            let local_proxy_port = match self.cursor_harness.proxy_port().await {
                Some(port) => port,
                None => self.store.port_settings().await?.proxy_port,
            };
            crate::network::reject_self_proxy(&settings.address, local_proxy_port)?;
        }
        let settings = self.store.set_proxy_settings(settings).await?;
        self.clients.invalidate().await;
        Ok(settings)
    }

    pub async fn tab_settings(&self) -> Result<TabSettings> {
        self.store.tab_settings().await
    }

    pub async fn cursor_model_aliases(&self) -> Result<BTreeMap<String, String>> {
        self.store.cursor_model_aliases().await
    }

    pub async fn set_cursor_model_aliases(
        &self,
        aliases: BTreeMap<String, String>,
    ) -> Result<BTreeMap<String, String>> {
        self.store.set_cursor_model_aliases(aliases).await
    }

    pub async fn set_tab_settings(&self, settings: TabSettings) -> Result<TabSettings> {
        self.cursor_harness.set_tab_settings(settings).await
    }

    pub async fn desktop_settings(&self) -> Result<DesktopSettings> {
        self.store.desktop_settings().await
    }

    pub async fn set_desktop_settings(&self, settings: DesktopSettings) -> Result<()> {
        self.store.set_desktop_settings(settings).await
    }

    pub async fn commit_settings(&self) -> Result<CommitSettings> {
        self.store.commit_settings().await
    }

    pub async fn set_commit_settings(&self, settings: CommitSettings) -> Result<CommitSettings> {
        self.store.set_commit_settings(settings).await
    }
}

/// 控制 API 会把已保存的敏感头值替换为 REDACTED_SECRET 占位符;更新时占位符
/// 视为「未修改」,从现有配置回填(头名按 HTTP 语义大小写不敏感),空串视为
/// 「清除」。非敏感头原样往返,用户在编辑器里的增删直接生效。返回回填的头数量。
fn restore_redacted_headers(next: &mut serde_json::Value, previous: &serde_json::Value) -> usize {
    let (Some(next), Some(previous)) = (next.as_object_mut(), previous.as_object()) else {
        return 0;
    };
    let mut restored = 0;
    for (name, value) in next.iter_mut() {
        if !crate::model::is_sensitive_header(name)
            || value.as_str() != Some(crate::model::REDACTED_SECRET)
        {
            continue;
        }
        if let Some(previous_value) = previous
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value)
        {
            *value = previous_value.clone();
            restored += 1;
        }
    }
    restored
}

fn official_call(trace: CursorRunTraceSummary) -> CallSummary {
    let model_id = trace.model_id.clone().unwrap_or_else(|| "Cursor".into());
    let ttfb = trace
        .first_response_at_ms
        .map(|value| (value - trace.received_at_ms).max(0));
    let duration = trace
        .finished_at_ms
        .map(|value| (value - trace.received_at_ms).max(0));
    let error = trace.error_message.clone();
    CallSummary {
        call: LlmCallSummary {
            call_id: format!("cursor:{}", trace.request_id),
            run_id: trace.request_id.clone(),
            conversation_id: trace
                .conversation_id
                .clone()
                .unwrap_or_else(|| trace.request_id.clone()),
            provider_call_index: 0,
            model_hash: None,
            provider_type: "cursor-official".into(),
            provider_url: "https://api2.cursor.sh".into(),
            request_type: "cursor-run-sse".into(),
            request_url: "https://api2.cursor.sh/agent.v1.AgentService/RunSSE".into(),
            model_id: model_id.clone(),
            display_name: model_id,
            reasoning_effort: None,
            fast: None,
            status: trace.status.clone(),
            finish_reason: None,
            created_at_ms: trace.received_at_ms,
            request_started_at_ms: Some(trace.received_at_ms),
            response_headers_at_ms: trace.first_response_at_ms,
            first_event_at_ms: trace.first_response_at_ms,
            first_text_at_ms: None,
            first_valid_response_at_ms: None,
            finished_at_ms: trace.finished_at_ms,
            queue_ms: None,
            ttfb_ms: ttfb,
            ttft_ms: None,
            ttfr_ms: None,
            duration_ms: duration,
            input_tokens: None,
            output_tokens: None,
            total_tokens: None,
            cache_read_tokens: None,
            cache_write_tokens: None,
            reasoning_tokens: None,
            usage: None,
            message_count: 0,
            tool_count: 0,
            request_bytes: Some(trace.request_bytes),
            response_bytes: trace.response_bytes,
            stream_event_count: trace.response_event_count,
            http_status: trace.http_status,
            error_kind: error.as_ref().map(|_| "cursor_official".into()),
            error_message: error,
            detailed: true,
        },
        call_kind: "cursor_official",
        route: "cursor_official",
    }
}

fn cursor_artifact(artifact: CursorRunTraceArtifact) -> CursorTraceArtifactDetail {
    let byte_count = artifact.data.len();
    let (encoding, data) = match readable_utf8(&artifact.data) {
        Some(value) => ("utf8", value.into()),
        None => ("base64", STANDARD.encode(&artifact.data)),
    };
    CursorTraceArtifactDetail {
        seq: artifact.seq,
        artifact_type: artifact.artifact_type,
        source: artifact.source,
        metadata: artifact.metadata,
        created_at_ms: artifact.created_at_ms,
        byte_count,
        encoding,
        data,
    }
}

fn readable_utf8(data: &[u8]) -> Option<&str> {
    let value = std::str::from_utf8(data).ok()?;
    value
        .chars()
        .all(|character| !character.is_control() || matches!(character, '\n' | '\r' | '\t'))
        .then_some(value)
}

async fn discover_models_from_endpoint(
    client: &reqwest::Client,
    provider_type: ProviderType,
    base_url: &str,
    api_key: &str,
    custom_headers: &serde_json::Value,
) -> Result<DiscoveredModels> {
    let mut models = match provider_type {
        ProviderType::OpenAiChat | ProviderType::OpenAiResponses => {
            openai_models(client, base_url, api_key, custom_headers).await?
        }
        ProviderType::Anthropic => {
            anthropic_models(client, base_url, api_key, custom_headers).await?
        }
        ProviderType::Plugin => {
            return Err(Error::Config(
                "plugin providers discover models through their plugin".into(),
            ))
        }
    };
    models.sort();
    models.dedup();
    Ok(DiscoveredModels { models })
}

fn model_discovery_url(base_url: &str) -> Result<Url> {
    let mut url = Url::parse(base_url)
        .map_err(|error| Error::Config(format!("invalid model request URL: {error}")))?;
    if url.host_str().is_none() {
        return Err(Error::Config(
            "model request URL must contain a host".into(),
        ));
    }
    // 在现有路径上追加，而不是整段替换：多数编程套餐的 API 挂在子路径下
    // （/api/anthropic、/coding、/api/paas/v4 等），直接 set_path("/v1/models")
    // 会把这些前缀吃掉，发现请求必然 404
    let path = url.path().trim_end_matches('/');
    let last = path.rsplit('/').next().unwrap_or("");
    let versioned = last.len() > 1
        && last.starts_with('v')
        && last[1..].bytes().all(|byte| byte.is_ascii_digit());
    let new_path = if let Some(parent) = path.strip_suffix("/chat/completions") {
        // 完整请求 URL：剥掉端点段（chat/completions 是两段），换成 models
        format!("{parent}/models")
    } else if let Some(parent) = path
        .strip_suffix("/responses")
        .or_else(|| path.strip_suffix("/messages"))
        .or_else(|| path.strip_suffix("/completions"))
    {
        format!("{parent}/models")
    } else if path.is_empty() {
        "/v1/models".to_string()
    } else if versioned {
        // 已带版本段（/v1、/api/v3、/api/paas/v4）：只补 models
        format!("{path}/models")
    } else {
        format!("{path}/v1/models")
    };
    url.set_path(&new_path);
    url.set_query(None);
    url.set_fragment(None);
    Ok(url)
}

fn model_discovery_urls(base_url: &str) -> Result<Vec<Url>> {
    let mut configured = Url::parse(base_url)
        .map_err(|error| Error::Config(format!("invalid model request URL: {error}")))?;
    let path = configured.path().trim_end_matches('/');
    let tail = path.rsplit('/').next().unwrap_or_default();
    if matches!(tail.to_ascii_lowercase().as_str(), "model" | "models") {
        configured.set_query(None);
        configured.set_fragment(None);
        return Ok(vec![configured]);
    }

    let primary = model_discovery_url(base_url)?;
    let versioned = tail.len() > 1
        && tail.starts_with('v')
        && tail[1..].bytes().all(|byte| byte.is_ascii_digit());
    let complete_request_url = [
        "/chat/completions",
        "/responses",
        "/messages",
        "/completions",
    ]
    .iter()
    .any(|suffix| path.to_ascii_lowercase().ends_with(suffix));
    if versioned || complete_request_url {
        return Ok(vec![primary]);
    }

    let Some(prefix) = primary.path().strip_suffix("/v1/models") else {
        return Ok(vec![primary]);
    };
    let mut fallback = primary.clone();
    fallback.set_path(&format!("{prefix}/models"));
    Ok(vec![primary, fallback])
}

async fn openai_models(
    client: &reqwest::Client,
    base_url: &str,
    api_key: &str,
    custom_headers: &serde_json::Value,
) -> Result<Vec<String>> {
    let mut last_error = None;
    for url in model_discovery_urls(base_url)? {
        match openai_models_at(client, url, api_key, custom_headers).await {
            Ok(models) => return Ok(models),
            Err(error) => last_error = Some(error),
        }
    }
    Err(last_error.unwrap_or_else(|| Error::Provider("no model discovery URL available".into())))
}

async fn openai_models_at(
    client: &reqwest::Client,
    url: Url,
    api_key: &str,
    custom_headers: &serde_json::Value,
) -> Result<Vec<String>> {
    let mut request = client.get(url);
    if !api_key.is_empty() {
        request = request.bearer_auth(api_key);
    }
    let response = apply_discovery_headers(request, custom_headers)?
        .send()
        .await?;
    let status = response.status();
    let body: serde_json::Value = response.json().await?;
    if !status.is_success() {
        return Err(Error::Provider(format!(
            "model discovery failed ({status}): {body}"
        )));
    }
    Ok(model_ids(body.get("data").unwrap_or(&body)))
}

async fn anthropic_models(
    client: &reqwest::Client,
    base_url: &str,
    api_key: &str,
    custom_headers: &serde_json::Value,
) -> Result<Vec<String>> {
    let mut last_error = None;
    for url in model_discovery_urls(base_url)? {
        match anthropic_models_at(client, url, api_key, custom_headers).await {
            Ok(models) => return Ok(models),
            Err(error) => last_error = Some(error),
        }
    }
    Err(last_error.unwrap_or_else(|| Error::Provider("no model discovery URL available".into())))
}

async fn anthropic_models_at(
    client: &reqwest::Client,
    url: Url,
    api_key: &str,
    custom_headers: &serde_json::Value,
) -> Result<Vec<String>> {
    let mut after_id = None::<String>;
    let mut found = BTreeSet::new();
    loop {
        let mut request = client
            .get(url.clone())
            .query(&[("limit", "100")])
            .header("anthropic-version", "2023-06-01");
        if !api_key.is_empty() {
            request = request.header("x-api-key", api_key);
        }
        if let Some(after_id) = &after_id {
            request = request.query(&[("after_id", after_id)]);
        }
        let response = apply_discovery_headers(request, custom_headers)?
            .send()
            .await?;
        let status = response.status();
        let body: serde_json::Value = response.json().await?;
        if !status.is_success() {
            return Err(Error::Provider(format!(
                "model discovery failed ({status}): {body}"
            )));
        }
        found.extend(model_ids(body.get("data").unwrap_or(&body)));
        if body.get("has_more").and_then(serde_json::Value::as_bool) != Some(true) {
            break;
        }
        after_id = body
            .get("last_id")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned);
        if after_id.is_none() {
            return Err(Error::Provider(
                "Anthropic model response has_more without last_id".into(),
            ));
        }
    }
    Ok(found.into_iter().collect())
}

fn model_ids(value: &serde_json::Value) -> Vec<String> {
    value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|item| match item {
            serde_json::Value::String(id) => Some(id.clone()),
            serde_json::Value::Object(object) => object
                .get("id")
                .or_else(|| object.get("name"))
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned),
            _ => None,
        })
        .collect()
}

fn estimate_output_tokens(output: &str) -> u64 {
    let words = output.split_whitespace().count() as u64;
    if words > 0 {
        words
    } else if output.is_empty() {
        0
    } else {
        (output.chars().count() as u64).div_ceil(4)
    }
}

fn apply_discovery_headers(
    mut request: reqwest::RequestBuilder,
    headers: &serde_json::Value,
) -> Result<reqwest::RequestBuilder> {
    let object = headers
        .as_object()
        .ok_or_else(|| Error::Config("custom headers must be an object".into()))?;
    for (name, value) in object {
        if name.eq_ignore_ascii_case("user-agent") {
            continue;
        }
        let value = value
            .as_str()
            .ok_or_else(|| Error::Config(format!("custom header {name} must be a string")))?;
        let name = HeaderName::try_from(name)
            .map_err(|error| Error::Config(format!("invalid header name: {error}")))?;
        let value = HeaderValue::try_from(value)
            .map_err(|error| Error::Config(format!("invalid header value: {error}")))?;
        request = request.header(name, value);
    }
    Ok(request)
}
#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use tokio_util::sync::CancellationToken;

    use crate::{
        model::{ModelConfig, ModelConfigInput, ModelInvocation, ModelType, ProjectedContent},
        plugin::{PluginRegistry, PluginRuntime},
        provider::{FinishReason, ModelEvent, Provider, ProviderStream},
        store::Store,
    };

    use super::{model_discovery_url, model_discovery_urls, ControlService};
    use crate::control::{AccessToken, AccessTokenSource};

    #[test]
    fn model_discovery_url_appends_to_path() {
        let cases = [
            (
                "https://api.deepseek.com",
                "https://api.deepseek.com/v1/models",
            ),
            (
                "https://open.bigmodel.cn/api/anthropic",
                "https://open.bigmodel.cn/api/anthropic/v1/models",
            ),
            (
                "https://api.kimi.com/coding",
                "https://api.kimi.com/coding/v1/models",
            ),
            (
                "https://api.moonshot.cn/v1",
                "https://api.moonshot.cn/v1/models",
            ),
            (
                "https://ark.cn-beijing.volces.com/api/v3",
                "https://ark.cn-beijing.volces.com/api/v3/models",
            ),
            (
                "https://open.bigmodel.cn/api/coding/paas/v4/chat/completions",
                "https://open.bigmodel.cn/api/coding/paas/v4/models",
            ),
            (
                "https://example.com:8443/arbitrary/v1/chat/completions",
                "https://example.com:8443/arbitrary/v1/models",
            ),
        ];
        for (base, expected) in cases {
            assert_eq!(
                model_discovery_url(base).unwrap().as_str(),
                expected,
                "base: {base}"
            );
        }
    }

    #[test]
    fn model_discovery_urls_fall_back_without_a_version() {
        let cases = [
            (
                "https://opencode.ai/zen/go/v1",
                vec!["https://opencode.ai/zen/go/v1/models"],
            ),
            (
                "https://opencode.ai/zen/go",
                vec![
                    "https://opencode.ai/zen/go/v1/models",
                    "https://opencode.ai/zen/go/models",
                ],
            ),
            (
                "https://api.example.com/openai/v1/models",
                vec!["https://api.example.com/openai/v1/models"],
            ),
        ];
        for (base, expected) in cases {
            let actual = model_discovery_urls(base)
                .unwrap()
                .into_iter()
                .map(|url| url.to_string())
                .collect::<Vec<_>>();
            assert_eq!(actual, expected, "base: {base}");
        }
    }

    #[tokio::test]
    async fn openai_model_discovery_uses_the_unversioned_fallback() {
        let app = axum::Router::new().route(
            "/proxy/models",
            axum::routing::get(|| async {
                axum::Json(serde_json::json!({ "data": [{ "id": "model-a" }] }))
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let models = super::openai_models(
            &reqwest::Client::new(),
            &format!("http://{address}/proxy"),
            "secret",
            &serde_json::json!({}),
        )
        .await
        .unwrap();

        assert_eq!(models, vec!["model-a"]);
        server.abort();
    }

    struct TestProvider {
        invocation: Arc<Mutex<Option<ModelInvocation>>>,
    }

    struct CancellationProvider {
        started: Arc<tokio::sync::Notify>,
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

    async fn create_test_model(store: &Store) -> ModelConfig {
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
                reasoning_effort: Some("medium".into()),
                effort_options: vec!["low".into(), "high".into()],
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
                anthropic_thinking_effort: None,
                thinking_budget_tokens: None,
            })
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn connectivity_test_uses_the_configured_llm_provider() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::connect(&format!(
            "sqlite://{}",
            directory.path().join("control.db").display()
        ))
        .await
        .unwrap();
        let invocation = Arc::new(Mutex::new(None));
        let model = create_test_model(&store).await;
        let plugin_runtime = PluginRuntime::managed().unwrap();
        let plugins =
            PluginRegistry::managed(store.clone(), plugin_runtime.clone(), "test".into()).unwrap();
        let clients = crate::network::NetworkClients::new(store.clone());
        let service = ControlService::new(
            store,
            Arc::new(TestProvider {
                invocation: invocation.clone(),
            }),
            plugin_runtime,
            plugins,
            clients,
            AccessToken::new("test".into(), AccessTokenSource::Generated),
        )
        .unwrap();

        let result = service
            .test_model(&model.model_hash, "test-id")
            .await
            .unwrap();

        assert_eq!(result.output, "OK");
        assert!(result.first_valid_response_ms.is_some());
        assert_eq!(result.output_tokens, 2);
        assert!(!result.tokens_estimated);
        assert!(result.tokens_per_second > 0.0);
        let invocation = invocation.lock().unwrap().clone().unwrap();
        assert_eq!(invocation.request.model.model_id, model.model_hash);
        assert!(invocation.request.model.reasoning.enabled);
        assert_eq!(
            invocation.request.model.reasoning.effort.as_deref(),
            Some("medium")
        );
        assert!(invocation.request.prompt.tools.is_empty());
        assert_eq!(invocation.request.history.len(), 1);
        assert!(matches!(
            &invocation.request.history[0].content,
            ProjectedContent::Parts(parts)
                if matches!(&parts[..], [crate::model::ContentPart::Text { text }] if text == "Output the numbers 1 through 120 separated by a single space. No commas, no newlines, no explanation.")
        ));
    }

    #[tokio::test]
    async fn connectivity_test_can_be_cancelled() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::connect(&format!(
            "sqlite://{}",
            directory.path().join("cancel.db").display()
        ))
        .await
        .unwrap();
        let model = create_test_model(&store).await;
        let started = Arc::new(tokio::sync::Notify::new());
        let plugin_runtime = PluginRuntime::managed().unwrap();
        let plugins =
            PluginRegistry::managed(store.clone(), plugin_runtime.clone(), "test".into()).unwrap();
        let clients = crate::network::NetworkClients::new(store.clone());
        let service = ControlService::new(
            store,
            Arc::new(CancellationProvider {
                started: started.clone(),
            }),
            plugin_runtime,
            plugins,
            clients,
            AccessToken::new("test".into(), AccessTokenSource::Generated),
        )
        .unwrap();
        let running_service = service.clone();
        let model_hash = model.model_hash.clone();
        let task =
            tokio::spawn(
                async move { running_service.test_model(&model_hash, "cancel-test").await },
            );

        started.notified().await;
        service.cancel_model_test("cancel-test");

        assert!(matches!(task.await.unwrap(), Err(crate::Error::Cancelled)));
        assert!(!service
            .model_tests
            .lock()
            .unwrap()
            .contains_key("cancel-test"));
    }

    #[test]
    fn connectivity_output_token_estimate_handles_words_and_empty_text() {
        assert_eq!(super::estimate_output_tokens("1 2 3"), 3);
        assert_eq!(super::estimate_output_tokens(""), 0);
    }

    fn service_for(store: crate::store::Store) -> ControlService {
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

    async fn model_with_headers(store: &crate::store::Store) -> crate::model::ModelConfig {
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
            reasoning_effort: None,
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
            anthropic_thinking_effort: None,
            thinking_budget_tokens: None,
        };
        store.create_model(&input).await.unwrap()
    }

    /// 编辑器往返:脱敏后的输入(占位符 api_key、占位符敏感头)不得改变已存密钥。
    #[tokio::test]
    async fn redacted_round_trip_preserves_api_key_and_headers() {
        let directory = tempfile::tempdir().unwrap();
        let store = crate::store::Store::connect(&format!(
            "sqlite://{}",
            directory.path().join("update.db").display()
        ))
        .await
        .unwrap();
        let service = service_for(store.clone());
        let created = model_with_headers(&store).await;
        let mut input = created.clone().redact_secrets().into_input();

        let updated = service
            .update_model(&created.model_hash, &input)
            .await
            .unwrap();
        assert_eq!(updated.api_key, "secret");
        assert_eq!(updated.custom_headers["Authorization"], "Bearer token");
        assert_eq!(updated.custom_headers["X-Tenant"], "tenant-a");

        // 轮换 API Key 时同样保留自定义头。
        input.api_key = "rotated".into();
        let rotated = service
            .update_model(&updated.model_hash, &input)
            .await
            .unwrap();
        assert_eq!(rotated.api_key, "rotated");
        assert_eq!(rotated.custom_headers["Authorization"], "Bearer token");
        assert_eq!(rotated.custom_headers["X-Tenant"], "tenant-a");
    }

    /// 显式编辑直接生效:改值、删键、清空全部。
    #[tokio::test]
    async fn explicit_header_edits_apply_directly() {
        let directory = tempfile::tempdir().unwrap();
        let store = crate::store::Store::connect(&format!(
            "sqlite://{}",
            directory.path().join("headers.db").display()
        ))
        .await
        .unwrap();
        let service = service_for(store.clone());
        let created = model_with_headers(&store).await;
        let mut input = created.clone().redact_secrets().into_input();
        input.custom_headers = serde_json::json!({
            "Authorization": "Bearer new",
            "X-Tenant": "tenant-b"
        });

        let updated = service
            .update_model(&created.model_hash, &input)
            .await
            .unwrap();
        assert_eq!(updated.custom_headers["Authorization"], "Bearer new");
        assert_eq!(updated.custom_headers["X-Tenant"], "tenant-b");

        input.custom_headers = serde_json::json!({"X-Tenant": "tenant-b"});
        let deleted = service
            .update_model(&updated.model_hash, &input)
            .await
            .unwrap();
        assert!(deleted.custom_headers.get("Authorization").is_none());
        assert_eq!(deleted.custom_headers["X-Tenant"], "tenant-b");
    }

    /// 显式清空语义:空串敏感头直接清除,不再回填;api_key 模型层要求非空,
    /// 清空会被校验拒绝(而不是静默回填)。
    #[tokio::test]
    async fn empty_secrets_clear_headers_but_api_key_stays_required() {
        let directory = tempfile::tempdir().unwrap();
        let store = crate::store::Store::connect(&format!(
            "sqlite://{}",
            directory.path().join("clear.db").display()
        ))
        .await
        .unwrap();
        let service = service_for(store.clone());
        let created = model_with_headers(&store).await;
        let mut input = created.clone().redact_secrets().into_input();

        input.custom_headers = serde_json::json!({
            "Authorization": "",
            "X-Tenant": "tenant-a"
        });
        let updated = service
            .update_model(&created.model_hash, &input)
            .await
            .unwrap();
        assert_eq!(updated.custom_headers["Authorization"], "");
        assert_eq!(updated.custom_headers["X-Tenant"], "tenant-a");
        assert_eq!(updated.api_key, "secret");

        input.api_key = String::new();
        let rejected = service.update_model(&created.model_hash, &input).await;
        assert!(matches!(rejected, Err(crate::Error::Config(_))));
    }

    /// 复制模型从存储中的未脱敏配置克隆,不经编辑器往返。
    #[tokio::test]
    async fn duplicate_clones_secrets_from_the_stored_model() {
        let directory = tempfile::tempdir().unwrap();
        let store = crate::store::Store::connect(&format!(
            "sqlite://{}",
            directory.path().join("duplicate.db").display()
        ))
        .await
        .unwrap();
        let service = service_for(store.clone());
        let created = model_with_headers(&store).await;

        let copy = service
            .duplicate_model(&created.model_hash, "Header Model 副本".into(), 2)
            .await
            .unwrap();

        assert_ne!(copy.model_hash, created.model_hash);
        assert_eq!(copy.display_name, "Header Model 副本");
        assert_eq!(copy.sort_order, 2);
        assert_eq!(copy.api_key, "secret");
        assert_eq!(copy.custom_headers["Authorization"], "Bearer token");
        assert_eq!(copy.custom_headers["X-Tenant"], "tenant-a");
    }

    #[tokio::test]
    async fn model_discovery_does_not_inherit_user_agent_or_request_body_settings() {
        type CapturedRequest = (
            axum::http::Method,
            axum::http::Uri,
            axum::http::HeaderMap,
            bytes::Bytes,
        );

        async fn models(
            axum::extract::State(sender): axum::extract::State<
                tokio::sync::mpsc::UnboundedSender<CapturedRequest>,
            >,
            request: axum::extract::Request,
        ) -> axum::Json<serde_json::Value> {
            let (parts, body) = request.into_parts();
            let body = axum::body::to_bytes(body, usize::MAX).await.unwrap();
            sender
                .send((parts.method, parts.uri, parts.headers, body))
                .unwrap();
            axum::Json(serde_json::json!({ "data": [{ "id": "model-a" }] }))
        }

        let (sender, mut requests) = tokio::sync::mpsc::unbounded_channel();
        let app = axum::Router::new()
            .route("/custom/models", axum::routing::get(models))
            .with_state(sender);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let directory = tempfile::tempdir().unwrap();
        let store = Store::connect(&format!(
            "sqlite://{}",
            directory.path().join("discovery.db").display()
        ))
        .await
        .unwrap();
        let plugin_runtime = PluginRuntime::managed().unwrap();
        let plugins =
            PluginRegistry::managed(store.clone(), plugin_runtime.clone(), "test".into()).unwrap();
        let clients = crate::network::NetworkClients::new(store.clone());
        let service = ControlService::new(
            store,
            Arc::new(TestProvider {
                invocation: Arc::new(Mutex::new(None)),
            }),
            plugin_runtime,
            plugins,
            clients,
            AccessToken::new("test".into(), AccessTokenSource::Generated),
        )
        .unwrap();
        let result = service
            .discover_models(&super::ModelDiscoveryInput {
                model_type: ModelType::OpenAi,
                base_url: format!("http://{address}/custom/responses"),
                api_key: "secret".into(),
                model_hash: None,
                custom_headers_enabled: true,
                custom_headers: serde_json::json!({
                    "uSeR-aGeNt": "inherited-user-agent",
                    "x-tenant": "tenant-a"
                }),
            })
            .await
            .unwrap();

        assert_eq!(result.models, vec!["model-a"]);
        let (method, uri, headers, body) = requests.recv().await.unwrap();
        assert_eq!(method, axum::http::Method::GET);
        // /custom/responses 剥掉端点段后是 /custom，发现地址为 /custom/models
        assert_eq!(uri.path(), "/custom/models");
        assert!(body.is_empty());
        assert!(headers.get(axum::http::header::USER_AGENT).is_none());
        assert_eq!(headers.get("x-tenant").unwrap(), "tenant-a");
        assert_eq!(
            headers.get(axum::http::header::AUTHORIZATION).unwrap(),
            "Bearer secret"
        );
        server.abort();
    }

    /// 回填存储密钥时,目标 URL 必须仍是模型自己配置的地址,
    /// 否则存储的密钥会被发往调用方指定的任意服务器。
    #[tokio::test]
    async fn discovery_with_stored_credentials_requires_the_configured_base_url() {
        type CapturedRequest = axum::http::HeaderMap;

        async fn models(
            axum::extract::State(sender): axum::extract::State<
                tokio::sync::mpsc::UnboundedSender<CapturedRequest>,
            >,
            request: axum::extract::Request,
        ) -> axum::Json<serde_json::Value> {
            sender.send(request.headers().clone()).unwrap();
            axum::Json(serde_json::json!({ "data": [{ "id": "model-a" }] }))
        }

        let (sender, mut requests) = tokio::sync::mpsc::unbounded_channel();
        let app = axum::Router::new()
            .route("/custom/models", axum::routing::get(models))
            .with_state(sender);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let directory = tempfile::tempdir().unwrap();
        let store = Store::connect(&format!(
            "sqlite://{}",
            directory.path().join("discovery-gate.db").display()
        ))
        .await
        .unwrap();
        let service = service_for(store.clone());
        let mut input = model_with_headers(&store).await.into_input();
        input.base_url = format!("http://{address}/custom/responses");
        let created = store.create_model(&input).await.unwrap();

        let discover = |base_url: String, api_key: String| super::ModelDiscoveryInput {
            model_type: ModelType::OpenAi,
            base_url,
            api_key,
            model_hash: Some(created.model_hash.clone()),
            custom_headers_enabled: false,
            custom_headers: serde_json::json!({}),
        };

        // 改了 base_url、密钥保留占位符(等回填)→ 拒绝。
        let rejected = service
            .discover_models(&discover(
                "https://attacker.example.com/v1/responses".into(),
                crate::model::REDACTED_SECRET.into(),
            ))
            .await;
        assert!(matches!(rejected, Err(crate::Error::Config(_))));

        // 空白与尾随斜杠差异不算改地址,回填的存储密钥正常发出。
        let allowed = service
            .discover_models(&discover(
                format!(" http://{address}/custom/responses/ "),
                crate::model::REDACTED_SECRET.into(),
            ))
            .await
            .unwrap();
        assert_eq!(allowed.models, vec!["model-a"]);
        let headers = requests.recv().await.unwrap();
        assert_eq!(
            headers.get(axum::http::header::AUTHORIZATION).unwrap(),
            "Bearer secret"
        );

        // 显式提供新密钥时不涉及存储密钥,允许任意地址。
        let allowed = service
            .discover_models(&discover(format!("http://{address}"), "fresh-key".into()))
            .await;
        // 未回填则不设限;该地址的 /v1/models 不存在,失败是 404 而非门禁拒绝。
        assert!(
            !matches!(allowed, Err(crate::Error::Config(_))),
            "{allowed:?}"
        );
        server.abort();
    }

    /// 编辑器往返:脱敏后的占位符在发现模型时由服务端从存储回填。
    #[tokio::test]
    async fn discovery_falls_back_to_stored_credentials_when_redacted() {
        type CapturedRequest = (axum::http::Uri, axum::http::HeaderMap);

        async fn models(
            axum::extract::State(sender): axum::extract::State<
                tokio::sync::mpsc::UnboundedSender<CapturedRequest>,
            >,
            request: axum::extract::Request,
        ) -> axum::Json<serde_json::Value> {
            let (parts, _) = request.into_parts();
            sender.send((parts.uri, parts.headers)).unwrap();
            axum::Json(serde_json::json!({ "data": [{ "id": "model-a" }] }))
        }

        let (sender, mut requests) = tokio::sync::mpsc::unbounded_channel();
        let app = axum::Router::new()
            .route("/custom/models", axum::routing::get(models))
            .with_state(sender);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let directory = tempfile::tempdir().unwrap();
        let store = Store::connect(&format!(
            "sqlite://{}",
            directory.path().join("redacted-discovery.db").display()
        ))
        .await
        .unwrap();
        let service = service_for(store.clone());
        let base_url = format!("http://{address}/custom/responses");
        let created = store
            .create_model(&ModelConfigInput {
                model_id: "model-a".into(),
                display_name: "Model A".into(),
                group_name: None,
                model_type: ModelType::OpenAi,
                base_url: base_url.clone(),
                use_full_url: true,
                api_key: "stored-key".into(),
                tooltip_data: "Model A".into(),
                sort_order: 0,
                reasoning_effort: None,
                effort_options: Vec::new(),
                context_options: Vec::new(),
                openai_endpoint: "/v1/responses".into(),
                openai_extra_params_enabled: false,
                openai_extra_params: serde_json::json!({}),
                custom_headers_enabled: true,
                custom_headers: serde_json::json!({
                    "X-Api-Key": "stored-header-key",
                    "X-Tenant": "tenant-a"
                }),
                anthropic_extra_params_enabled: false,
                anthropic_extra_params: serde_json::json!({}),
                context_window_tokens: None,
                max_completion_tokens: None,
                anthropic_max_tokens: None,
                anthropic_thinking_effort: None,
                thinking_budget_tokens: None,
            })
            .await
            .unwrap();

        let result = service
            .discover_models(&super::ModelDiscoveryInput {
                model_type: ModelType::OpenAi,
                base_url,
                api_key: crate::model::REDACTED_SECRET.into(),
                model_hash: Some(created.model_hash.clone()),
                custom_headers_enabled: true,
                custom_headers: serde_json::json!({
                    "X-Api-Key": crate::model::REDACTED_SECRET,
                    "X-Tenant": "tenant-a"
                }),
            })
            .await
            .unwrap();

        assert_eq!(result.models, vec!["model-a"]);
        let (uri, headers) = requests.recv().await.unwrap();
        assert_eq!(uri.path(), "/custom/models");
        assert_eq!(
            headers.get(axum::http::header::AUTHORIZATION).unwrap(),
            "Bearer stored-key"
        );
        assert_eq!(headers.get("x-api-key").unwrap(), "stored-header-key");
        assert_eq!(headers.get("x-tenant").unwrap(), "tenant-a");
        server.abort();
    }
}
