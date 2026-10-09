//! Implements control API routing and shared state.
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

use base64::{engine::general_purpose::STANDARD, Engine};
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::{
    control::auth::{AccessToken, AccessTokenSource},
    local_app::CursorHarness,
    model::{
        CursorRunTraceArtifact, CursorRunTraceSummary, LlmCallRequest, LlmCallResponseChunk,
        LlmCallSummary, ModelConfig, ModelConfigInput, Overview,
    },
    plugin::{PluginDescriptor, PluginRegistry, PluginRuntime, PluginRuntimeStatus},
    provider::Provider,
    store::{
        AppApiSettings, CommitSettings, DesktopSettings, PluginModelOverride, PortSettings,
        ProxySettings, ProxySettingsInput, Store, TabSettings,
    },
    Error, Result,
};

#[derive(Clone)]
pub struct ControlService {
    pub(super) store: Store,
    pub(super) cursor_harness: CursorHarness,
    pub(super) provider: Arc<dyn Provider>,
    pub(super) plugin_runtime: PluginRuntime,
    pub(super) plugins: PluginRegistry,
    pub(super) clients: crate::network::NetworkClients,
    access_token: AccessToken,
    pub(super) model_tests: Arc<Mutex<BTreeMap<String, CancellationToken>>>,
}

#[derive(Clone, Debug, Serialize)]
pub struct AccessTokenView {
    pub token: String,
    pub source: AccessTokenSource,
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

/// 本地存储占用：数据库文件与可重建缓存。
#[derive(Clone, Copy, Debug, Default, Serialize)]
pub struct StorageStatistics {
    /// 数据库文件占用（页数 × 页大小）。
    pub bytes: i64,
    /// 可重建缓存占用：semble 索引与克隆仓库、网页抓取结果。
    pub cache_bytes: i64,
}

/// 手动触发的存储清理请求。
#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "snake_case", tag = "mode")]
pub enum StorageCleanupRequest {
    /// 仅清除缓存：数据库详细记录、不可达历史与可重建的本地缓存。
    Cache,
    /// 清除缓存和调用记录：清空全部调用与会话数据，保留模型配置、
    /// 应用设置、插件环境/登录状态、用户规则与嵌入模型权重。
    Reset,
}

/// 手动触发的存储清理结果。
#[derive(Clone, Copy, Debug, Default, Serialize)]
pub struct StorageCleanup {
    /// 清理后的存储占用。
    pub storage: StorageStatistics,
    /// 本次释放的字节数：数据库文件减少量与已删除缓存之和。
    pub freed_bytes: i64,
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
            model_tests: super::connectivity::new_model_tests_registry(),
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

    pub async fn install_plugin(
        &self,
        path: &std::path::Path,
        replace: bool,
    ) -> Result<crate::plugin::InstallPluginResponse> {
        self.plugins.install_plugin(path, replace).await
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

    pub async fn clear_plugin_data(&self, plugin_id: &str) -> Result<()> {
        self.plugins.clear_data(plugin_id).await
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
        mut over: PluginModelOverride,
    ) -> Result<()> {
        let _write = self.plugins.lock_model_writes().await;
        if over.effort_options.is_none() && over.default_effort.is_some() {
            let model = self
                .plugins
                .all_models()
                .await?
                .into_iter()
                .find(|model| model.id == id)
                .ok_or_else(|| Error::RunNotFound(format!("plugin model {id}")))?;
            over.effort_options = Some(model.effort_options);
        }
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
        timezone_offset_minutes: Option<i32>,
    ) -> Result<Overview> {
        self.store
            .overview(
                start_ms,
                end_ms,
                model_hashes,
                bucket_ms,
                timezone_offset_minutes,
            )
            .await
    }

    pub async fn create_models(&self, models: &[ModelConfigInput]) -> Result<Vec<ModelConfig>> {
        let _write = self.plugins.lock_model_writes().await;
        self.store
            .create_models_checked(models, &self.plugins.all_models().await?)
            .await
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
        let _write = self.plugins.lock_model_writes().await;
        self.store
            .update_model_checked(model_hash, &input, &self.plugins.all_models().await?)
            .await
    }

    /// 分组设置整批保存:读现有配置 → 只合并用户编辑字段 → 占位符密钥回填 →
    /// 统一归一校验 → 单事务全部生效或整体回滚。响应保持脱敏。
    pub async fn update_model_group(
        &self,
        edits: &[crate::model::ModelGroupEdit],
    ) -> Result<Vec<ModelConfig>> {
        let _write = self.plugins.lock_model_writes().await;
        let models = self
            .store
            .update_models_group(edits, &self.plugins.all_models().await?)
            .await?;
        Ok(models)
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
        input.model_id.clear();
        Ok(self.create_models(&[input]).await?.remove(0))
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
                .standalone_cursor_traces(limit)
                .await?
                .into_iter()
                .map(cursor_trace_call),
        );
        calls.sort_by_key(|call| std::cmp::Reverse(call.call.created_at_ms));
        calls.truncate(limit.clamp(1, 500) as usize);
        Ok(calls)
    }

    pub async fn call(&self, call_id: &str) -> Result<CallDetail> {
        if let Some(call) = self.store.llm_call(call_id).await? {
            let cursor_trace = self.cursor_trace_for_run(&call.run_id).await?;
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
            .ok_or_else(|| Error::RunNotFound(format!("call {call_id}")))?;
        Ok(CallDetail {
            call: cursor_trace_call(trace.clone()),
            request: None,
            response_chunks: Vec::new(),
            cursor_trace: Some(self.cursor_trace_detail_from(trace).await?),
        })
    }

    /// Provider 调用属于一次本地 Run;Run 通过 `cursor_request_id` 关联 Cursor 追踪。
    async fn cursor_trace_for_run(&self, run_id: &str) -> Result<Option<CursorTraceDetail>> {
        let Some(request_id) = self.store.run_cursor_request_id(run_id).await? else {
            return Ok(None);
        };
        self.cursor_trace_detail(&request_id).await
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

    pub async fn statistics_storage(&self) -> Result<StorageStatistics> {
        Ok(StorageStatistics {
            bytes: self.store.database_bytes().await?,
            cache_bytes: crate::search::cache_bytes()? as i64,
        })
    }

    /// 清理数据库详细记录、不可达历史与空闲页，并删除可重建的本地缓存。
    pub async fn clean_storage(&self) -> Result<StorageCleanup> {
        let database = self.store.clean_database().await?;
        let freed_cache = crate::search::clear_caches()? as i64;
        Ok(StorageCleanup {
            storage: StorageStatistics {
                bytes: database.bytes,
                cache_bytes: crate::search::cache_bytes()? as i64,
            },
            freed_bytes: database.freed_bytes + freed_cache,
        })
    }

    /// 按模式清理存储：`cache` 走常规清理；`reset` 在其基础上进一步
    /// 清空全部调用与会话数据，只保留模型配置、应用设置、插件环境
    /// （含登录账户）、用户规则与嵌入模型权重。
    pub async fn clean_storage_by_mode(
        &self,
        request: StorageCleanupRequest,
    ) -> Result<StorageCleanup> {
        match request {
            StorageCleanupRequest::Cache => self.clean_storage().await,
            StorageCleanupRequest::Reset => {
                let database = self.store.reset_database().await?;
                let freed_cache = crate::search::clear_caches()? as i64;
                Ok(StorageCleanup {
                    storage: StorageStatistics {
                        bytes: database.bytes,
                        cache_bytes: crate::search::cache_bytes()? as i64,
                    },
                    freed_bytes: database.freed_bytes + freed_cache,
                })
            }
        }
    }

    pub async fn proxy_settings(&self) -> Result<ProxySettings> {
        self.store.proxy_settings().await
    }

    pub async fn set_proxy_settings(&self, settings: ProxySettingsInput) -> Result<ProxySettings> {
        if settings.mode.is_custom() {
            // 自环检查只用实际监听端口(运行中的 harness);
            // 持久化端口在端口随机回退窗口内可能是上一轮的旧值,
            // 不能作为本轮自环判定的依据(不为此新增持久化字段)。
            if let Some(port) = self.cursor_harness.proxy_port().await {
                crate::network::reject_self_proxy(&settings.address, port)?;
            }
        }
        let settings = self.store.set_proxy_settings(settings).await?;
        self.clients.invalidate().await;
        Ok(settings)
    }

    pub async fn tab_settings(&self) -> Result<TabSettings> {
        self.store.tab_settings().await
    }

    pub async fn set_tab_settings(&self, settings: TabSettings) -> Result<TabSettings> {
        self.cursor_harness.set_tab_settings(settings).await
    }

    pub async fn desktop_settings(&self) -> Result<DesktopSettings> {
        self.store.desktop_settings().await
    }

    pub async fn app_api_settings(&self) -> Result<AppApiSettings> {
        self.store.app_api_settings().await
    }

    pub async fn set_app_api_settings(&self, settings: AppApiSettings) -> Result<AppApiSettings> {
        self.store.set_app_api_settings(settings).await
    }

    pub async fn set_desktop_settings(&self, settings: DesktopSettings) -> Result<()> {
        self.store.set_desktop_settings(settings).await
    }

    pub async fn commit_settings(&self) -> Result<CommitSettings> {
        self.store.commit_settings().await
    }

    pub async fn set_commit_settings(&self, settings: CommitSettings) -> Result<CommitSettings> {
        let _write = self.plugins.lock_model_writes().await;
        self.store
            .set_commit_settings_checked(settings, &self.plugins.configured_models().await)
            .await
    }
}

/// 控制 API 会把已保存的敏感头值替换为 REDACTED_SECRET 占位符;更新时占位符
/// 视为「未修改」,从现有配置回填(头名按 HTTP 语义大小写不敏感),空串视为
/// 「清除」。非敏感头原样往返,用户在编辑器里的增删直接生效。返回回填的头数量。
pub(super) fn restore_redacted_headers(
    next: &mut serde_json::Value,
    previous: &serde_json::Value,
) -> usize {
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

fn cursor_trace_call(trace: CursorRunTraceSummary) -> CallSummary {
    let official = trace.route == "cursor_official";
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
            provider_type: if official {
                "cursor-official".into()
            } else {
                "cursor-transport".into()
            },
            provider_url: if official {
                "https://api2.cursor.sh".into()
            } else {
                "local://cursor".into()
            },
            request_type: if official {
                "cursor-run-sse".into()
            } else {
                "cursor-transport".into()
            },
            request_url: if official {
                "https://api2.cursor.sh/agent.v1.AgentService/RunSSE".into()
            } else {
                "/aiserver.v1.BidiService/BidiAppend".into()
            },
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
            error_kind: error.as_ref().map(|_| trace.route.clone()),
            error_message: error,
            detailed: trace.detailed,
        },
        call_kind: if official {
            "cursor_official"
        } else {
            "cursor_transport"
        },
        route: if official {
            "cursor_official"
        } else {
            "local_byok"
        },
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

pub(super) fn empty_json_object_ref() -> &'static serde_json::Value {
    static EMPTY: std::sync::OnceLock<serde_json::Value> = std::sync::OnceLock::new();
    EMPTY.get_or_init(|| serde_json::json!({}))
}

#[cfg(test)]
mod tests {
    use crate::model::{
        ConversationId, ModelSpec, NewLlmCall, PreparedRun, PromptSpec, ProviderType, RunAction,
        RunId, RunKind,
    };
    use crate::store::Store;

    use super::super::test_support::{model_with_headers, service_for};

    #[tokio::test]
    async fn provider_call_detail_links_trace_through_the_runs_cursor_request_id() {
        let store = Store::connect("sqlite::memory:").await.unwrap();
        store.set_detailed_logging(true).await.unwrap();
        store
            .start_cursor_trace_if_detailed(
                "cursor-request",
                Some("conversation"),
                "local_byok",
                Some("cursor-model"),
            )
            .await
            .unwrap();
        store
            .append_cursor_trace_artifact(
                "cursor-request",
                "client_message",
                "cursor_client",
                br#"{"runRequest":{}}"#,
                &serde_json::json!({"message_type": "run_request"}),
            )
            .await
            .unwrap();

        let conversation_id = ConversationId::new("conversation");
        let base_checkpoint_id = store.ensure_conversation(&conversation_id).await.unwrap();
        store
            .claim_run(&PreparedRun {
                run_id: RunId::new("provider-run"),
                cursor_request_id: Some("cursor-request".into()),
                conversation_id,
                kind: RunKind::Root,
                model: ModelSpec::new("provider-model"),
                prompt: PromptSpec {
                    instructions: String::new(),
                    tools: Vec::new(),
                },
                initial_messages: Vec::new(),
                action: RunAction::Start,
                base_checkpoint_id,
                background_follow_up: false,
            })
            .await
            .unwrap();
        let service = service_for(store.clone());
        // 运行中的本地 trace 不单独成行:首个 provider 调用行出现前宁可无行,
        // 也不让列表先显示 trace 之后再替换成调用行。
        assert!(service.calls(10).await.unwrap().is_empty());
        assert!(service
            .call("cursor:cursor-request")
            .await
            .unwrap()
            .cursor_trace
            .is_some());

        store
            .start_llm_call(&NewLlmCall {
                call_id: "provider-call".into(),
                run_id: "provider-run".into(),
                conversation_id: "conversation".into(),
                provider_call_index: 0,
                model_hash: "plugin:test/provider-model".into(),
                provider_type: ProviderType::Plugin,
                provider_url: "plugin://test".into(),
                request_type: ProviderType::Plugin,
                request_url: "plugin://test".into(),
                model_id: "provider-model".into(),
                display_name: "Provider Model".into(),
                reasoning_effort: None,
                fast: false,
                message_count: 1,
                tool_count: 0,
                detailed: true,
            })
            .await
            .unwrap();

        let linked = service.calls(10).await.unwrap();
        assert_eq!(linked.len(), 1);
        assert_eq!(linked[0].call.call_id, "provider-call");
        let detail = service.call("provider-call").await.unwrap();
        let trace = detail
            .cursor_trace
            .expect("provider call links to Cursor trace");
        assert_eq!(trace.trace.request_id, "cursor-request");
        assert_eq!(trace.artifacts.len(), 1);
        assert_eq!(trace.artifacts[0].artifact_type, "client_message");
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
    /// 副本清空上游模型名，以随机身份原像创建可编辑草稿；填写上游模型后转为常规 ID。
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
        let second = service
            .duplicate_model(&created.model_hash, "Another copy".into(), 3)
            .await
            .unwrap();
        assert_ne!(copy.model_hash, second.model_hash);
        assert!(copy.model_id.is_empty());
        let mut draft = copy.clone().into_input();
        draft.display_name = "Renamed draft".into();
        let saved = service
            .update_model(&copy.model_hash, &draft)
            .await
            .unwrap();
        assert_eq!(saved.model_hash, copy.model_hash);
        draft.model_id = "new-upstream-model".into();
        let completed = service
            .update_model(&copy.model_hash, &draft)
            .await
            .unwrap();
        assert_ne!(completed.model_hash, copy.model_hash);
        assert_eq!(
            completed.model_hash,
            crate::model::model_hash(&draft).unwrap()
        );

        assert_ne!(copy.model_hash, created.model_hash);
        assert_eq!(copy.display_name, "Header Model 副本");
        assert_eq!(copy.sort_order, 2);
        assert_eq!(copy.api_key, "secret");
        assert_eq!(copy.custom_headers["Authorization"], "Bearer token");
        assert_eq!(copy.custom_headers["X-Tenant"], "tenant-a");
    }

    /// 保存自定义代理设置时,自环检查使用 harness 实际监听的端口:
    /// 持久化端口在随机回退窗口内可能是上一轮的旧值,不参与判定。
    #[tokio::test]
    async fn proxy_settings_self_loop_check_uses_the_running_port_only() {
        let directory = tempfile::tempdir().unwrap();
        let store = crate::store::Store::connect(&format!(
            "sqlite://{}",
            directory.path().join("proxy.db").display()
        ))
        .await
        .unwrap();
        // 上一轮持久化过端口 40721,但本轮 harness 尚未启动(随机回退窗口)。
        store.set_proxy_port(40721).await.unwrap();
        let service = service_for(store.clone());
        let address = crate::store::ProxySettingsInput {
            mode: crate::store::ProxyMode::Custom,
            address: "http://127.0.0.1:40721".into(),
            auth_enabled: false,
            username: String::new(),
            password: None,
        };
        // harness 未运行:持久化端口不再参与判定,保存被放行
        // (本轮 harness 可能绑定任意随机端口,旧值无法代表实际监听端口)。
        assert!(service.set_proxy_settings(address.clone()).await.is_ok());
    }
}
