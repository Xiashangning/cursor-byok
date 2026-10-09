//! Orchestrates plugin capabilities: resources, model catalogs, and invocation.
use std::{collections::HashMap, path::Path, sync::Arc, time::Duration};

use async_stream::try_stream;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use serde::Serialize;
use sha2::{Digest, Sha256};
use tokio::sync::{Mutex, RwLock};
use tokio_util::sync::CancellationToken;

use super::{
    catalog::{PluginCatalog, PluginEntry},
    data::PluginDataStore,
    descriptor::{
        ModelNote, PluginDescriptor, PluginModelDescriptor, PluginProviderDescriptor,
        PluginResourceDescriptor, PluginResourceView, ProviderDefinition, ResourceActionResponse,
        ResourceActionResult, ResourceDefinition, ResourcePresentation, OAUTH2_ADD_METHOD,
        OAUTH2_AUTHORIZATION_CODE_ADD_METHOD,
    },
    oauth_callback::{self, CallbackHandle, CallbackOutcome, CallbackRequest},
    quota,
    runtime::PluginRuntime,
    selection,
    state::{now_ms, PluginStateStore, ResourceDraft, ResourcePatch, ResourceRecord, StoredModel},
    wire,
    worker::{PluginWorker, WorkerStreamItem},
};
use crate::{
    model::ModelInvocation,
    provider::ProviderStream,
    provider::{CallRecorder, ModelEvent},
    store::Store,
    Error, Result,
};

const OAUTH_SLOW_DOWN_STEP_MS: i64 = 5_000;
const MAX_IMPORT_DRAFTS: usize = 256;
/// 插件资源额度的后台刷新周期。
const QUOTA_REFRESH_INTERVAL: Duration = Duration::from_secs(60);

#[derive(Clone)]
pub struct PluginRegistry {
    inner: Arc<RegistryInner>,
}

struct RegistryInner {
    store: Store,
    runtime: PluginRuntime,
    catalog: PluginCatalog,
    state: PluginStateStore,
    entries: RwLock<Option<Vec<PluginEntry>>>,
    workers: Mutex<HashMap<String, Arc<PluginWorker>>>,
    oauth_sessions: Mutex<HashMap<String, OAuthSession>>,
    /// Cursor 模型目录消费的额度块,key 为 `{plugin_id}/{resource_type}`。
    quota_summaries: RwLock<HashMap<String, String>>,
    /// 插件模型的动态备注(限免、额度消耗倍率等),key 为 `{plugin_id}/{provider_id}`,
    /// 值为模型 ID → 备注文本;与额度摘要同一轮 60s 刷新。
    model_notes: RwLock<HashMap<String, HashMap<String, String>>>,
    model_writes: Mutex<()>,
    rr_counter: std::sync::atomic::AtomicUsize,
}

struct OAuthSession {
    plugin_id: String,
    resource_type: String,
    method_id: String,
    session: serde_json::Value,
    expires_at_ms: i64,
    poll_interval_ms: i64,
    next_poll_at_ms: i64,
    flow: OAuthFlow,
}

enum OAuthFlow {
    DeviceCode,
    AuthorizationCode {
        redirect_uri: String,
        code_verifier: String,
        callback: CallbackHandle,
    },
}

enum OAuthPollWork {
    DeviceCode {
        plugin_id: String,
        resource_type: String,
        method_id: String,
        session: serde_json::Value,
        poll_interval_ms: i64,
    },
    AuthorizationCode {
        plugin_id: String,
        resource_type: String,
        method_id: String,
        session: serde_json::Value,
        redirect_uri: String,
        code_verifier: String,
        callback_request: CallbackRequest,
    },
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OAuthBeginResponse {
    pub session_id: String,
    pub user_code: Option<String>,
    pub verification_url: String,
    pub verification_url_complete: Option<String>,
    pub expires_at_ms: i64,
    pub poll_interval_ms: i64,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase", tag = "status")]
pub enum OAuthPollResponse {
    #[serde(rename_all = "camelCase")]
    Pending { poll_interval_ms: i64 },
    #[serde(rename_all = "camelCase")]
    Completed {
        added: usize,
        updated: usize,
        model_sync_error: Option<String>,
    },
    #[serde(rename_all = "camelCase")]
    Denied { message: Option<String> },
    #[serde(rename_all = "camelCase")]
    Failed { message: String },
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "status", rename_all = "camelCase")]
pub enum InstallPluginResponse {
    Installed {
        id: String,
        name: String,
        replaced: bool,
    },
    Exists {
        id: String,
        name: String,
    },
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportResponse {
    pub added: usize,
    pub updated: usize,
    pub warnings: Vec<String>,
    pub model_sync_error: Option<String>,
}

impl PluginRegistry {
    pub fn managed(store: Store, runtime: PluginRuntime, app_version: String) -> Result<Self> {
        let data = PluginDataStore::managed()?;
        Ok(Self {
            inner: Arc::new(RegistryInner {
                store,
                runtime,
                catalog: PluginCatalog::managed(app_version)?,
                state: PluginStateStore::new(data),
                entries: RwLock::new(None),
                workers: Mutex::new(HashMap::new()),
                oauth_sessions: Mutex::new(HashMap::new()),
                quota_summaries: RwLock::new(HashMap::new()),
                model_notes: RwLock::new(HashMap::new()),
                model_writes: Mutex::new(()),
                rr_counter: std::sync::atomic::AtomicUsize::new(0),
            }),
        })
    }

    /// Covers validation and persistence across both model sources.
    pub(crate) async fn lock_model_writes(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.inner.model_writes.lock().await
    }

    /// Full identity inventory, including disabled/unconfigured providers.
    pub(crate) async fn all_models(&self) -> Result<Vec<PluginModelDescriptor>> {
        let Some(executable) = self.inner.runtime.executable() else {
            return Ok(Vec::new());
        };
        let mut models = Vec::new();
        for entry in self.entries(&executable).await {
            for provider in &entry.definition.providers {
                for model in self
                    .inner
                    .state
                    .models(&entry.manifest.id, &provider.id)
                    .await?
                {
                    models.push(PluginModelDescriptor::new(
                        &entry.manifest.id,
                        &entry.manifest.name,
                        &entry.icon,
                        provider,
                        &model,
                    ));
                }
            }
        }
        Ok(models)
    }

    pub async fn install_plugin(
        &self,
        path: &std::path::Path,
        replace: bool,
    ) -> Result<InstallPluginResponse> {
        let executable = self.executable()?;
        match self
            .inner
            .catalog
            .install_user_plugin(path, &executable, replace)
            .await?
        {
            super::user_install::PrepareOutcome::Exists { id, name } => {
                Ok(InstallPluginResponse::Exists { id, name })
            }
            super::user_install::PrepareOutcome::Pending(pending) => {
                let id = pending.id.clone();
                let name = pending.name.clone();
                let replaced = pending.replaced;
                if let Some(worker) = self.inner.workers.lock().await.remove(&id) {
                    worker.stop().await;
                }
                pending.commit()?;
                *self.inner.entries.write().await = None;
                Ok(InstallPluginResponse::Installed { id, name, replaced })
            }
        }
    }

    pub async fn plugins(&self) -> Vec<PluginDescriptor> {
        let Some(executable) = self.inner.runtime.executable() else {
            return self
                .inner
                .catalog
                .manifests()
                .into_iter()
                .map(|(manifest, icon)| PluginDescriptor {
                    id: manifest.id,
                    name: manifest.name,
                    version: manifest.version,
                    author: manifest.author,
                    icon,
                    providers: Vec::new(),
                    resources: Vec::new(),
                })
                .collect();
        };
        let mut plugins = Vec::new();
        for entry in self.entries(&executable).await {
            plugins.push(self.descriptor(&entry, &executable).await);
        }
        plugins
    }

    /// 已满足调用条件的全部插件模型;每个模型独立进入 Cursor 目录。
    pub async fn configured_models(&self) -> Vec<PluginModelDescriptor> {
        let Some(executable) = self.inner.runtime.executable() else {
            return Vec::new();
        };
        let disabled_models = self
            .inner
            .store
            .disabled_plugin_models()
            .await
            .unwrap_or_default();
        let overrides = self
            .inner
            .store
            .plugin_model_overrides()
            .await
            .unwrap_or_default();
        let disabled_accounts = self
            .inner
            .store
            .disabled_plugin_accounts()
            .await
            .unwrap_or_default();
        let mut models = Vec::new();
        for entry in self.entries(&executable).await {
            for provider in &entry.definition.providers {
                let stored = self
                    .inner
                    .state
                    .models(&entry.manifest.id, &provider.id)
                    .await
                    .unwrap_or_default();
                if !self
                    .provider_configured(&entry.manifest.id, provider, &stored, &disabled_accounts)
                    .await
                {
                    continue;
                }
                models.extend(stored.iter().filter_map(|model| {
                    let descriptor = PluginModelDescriptor::new(
                        &entry.manifest.id,
                        &entry.manifest.name,
                        &entry.icon,
                        provider,
                        model,
                    );
                    if disabled_models.contains(&descriptor.id) {
                        return None;
                    }
                    let descriptor = match overrides.get(&descriptor.id) {
                        Some(over) => descriptor.with_override(over),
                        None => descriptor,
                    };
                    Some(descriptor)
                }));
            }
        }
        models
    }

    /// 插件模型的统一 Provider 流:按亲和顺序选择候选账号,经 Worker 执行,
    /// 事件与内置 Provider 走同一管道。资源错误(额度/授权)在尚未发出任何
    /// 事件时按候选顺序换账号重试;已发出事件后不再切换,避免重复输出。
    /// 执行描述由路由方从目录解析后透传,此处不再重复解析。
    pub fn stream_model(
        &self,
        descriptor: PluginModelDescriptor,
        invocation: ModelInvocation,
        cancellation: CancellationToken,
        recorder: CallRecorder,
    ) -> ProviderStream {
        let registry = self.clone();
        Box::pin(try_stream! {
            let plugin_id = descriptor.plugin_id.clone();
            let provider_id = descriptor.provider_id.clone();
            let executable = registry.executable()?;
            let entry = registry.find_entry(&executable, &plugin_id).await?;
            let provider = find_provider(&entry, &provider_id)?;
            let stored = registry.inner.state.models(&plugin_id, &provider_id).await?
                .into_iter()
                .find(|model| model.id == descriptor.model_id)
                .ok_or_else(|| Error::RunNotFound(format!("plugin model {}", descriptor.id)))?;
            // 无资源 Provider 只有一次尝试;有资源 Provider 以会话 ID 为
            // 亲和键得到有序候选,首选与故障转移顺序一致。
            let candidates: Vec<(String, ResourceRecord)> = match &provider.resource_type {
                Some(resource_type) => registry
                    .select_resources(&plugin_id, resource_type, Some(&invocation.conversation_id))
                    .await?
                    .into_iter()
                    .map(|record| (resource_type.clone(), record))
                    .collect(),
                None => Vec::new(),
            };
            let request = wire::llm_request(&invocation)?;
            let worker = registry.worker(&entry, &executable).await;
            yield ModelEvent::Start { model_call_id: invocation.call_id.clone() };
            let attempts = candidates.len().max(1);
            let mut attempt = 0;
            loop {
                let resource = candidates.get(attempt);
                let params = serde_json::json!({
                    "providerId": provider_id,
                    "model": stored.snapshot(),
                    "resource": resource.map(|(resource_type, record)| record.snapshot(resource_type)),
                    "request": request,
                });
                let mut items = worker.invoke_streaming("provider.invoke", params, cancellation.clone(), Some(recorder.clone())).await?;
                let mut emitted = false;
                let mut result_value: Option<serde_json::Value> = None;
                while let Some(item) = items.recv().await {
                    match item {
                        WorkerStreamItem::Event(event) => {
                            emitted = true;
                            yield wire::model_event(&event)?;
                        }
                        WorkerStreamItem::Result(result) => {
                            result_value = Some(result?);
                            break;
                        }
                    }
                }
                let Some(value) = result_value else {
                    Err(Error::Provider(format!("plugin '{plugin_id}' worker stopped mid-stream")))?
                };
                let status = value.get("status").and_then(serde_json::Value::as_str).unwrap_or_default();
                let patch = value.get("patch")
                    .filter(|patch| !patch.is_null())
                    .map(|patch| serde_json::from_value::<ResourcePatch>(patch.clone()))
                    .transpose()?;
                if let (Some(patch), Some((resource_type, record))) = (patch, resource) {
                    registry
                        .inner
                        .state
                        .apply_patch(&plugin_id, resource_type, &record.id, patch)
                        .await?;
                }
                match status {
                    "completed" => return,
                    "resource-error" => {
                        let message = value.get("message")
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or("plugin provider call failed")
                            .to_owned();
                        if selection::can_failover(emitted, attempt, attempts) {
                            tracing::warn!(
                                plugin = %plugin_id,
                                account = %resource.map(|(_, record)| record.key.as_str()).unwrap_or_default(),
                                %message,
                                "plugin account failed before any event; failing over to the next candidate"
                            );
                            attempt += 1;
                            continue;
                        }
                        Err(Error::Provider(message))?;
                    }
                    "request-error" => {
                        let message = value.get("message")
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or("plugin provider call failed");
                        Err(Error::Provider(message.to_owned()))?;
                    }
                    status => {
                        Err(Error::Protocol(format!("unknown plugin provider result: {status}")))?;
                    }
                }
            }
        })
    }

    pub async fn oauth_begin(
        &self,
        plugin_id: &str,
        resource_type: &str,
        method_id: &str,
    ) -> Result<OAuthBeginResponse> {
        let executable = self.executable()?;
        let entry = self.find_entry(&executable, plugin_id).await?;
        let resource = find_resource(&entry, resource_type)?;
        let method = resource
            .add
            .iter()
            .find(|method| method.id == method_id)
            .ok_or_else(|| {
                Error::Config(format!(
                    "plugin '{plugin_id}' does not define OAuth method '{method_id}'"
                ))
            })?;
        self.inner.oauth_sessions.lock().await.retain(|_, session| {
            session.plugin_id != plugin_id
                || session.resource_type != resource_type
                || session.method_id != method_id
        });
        let worker = self.worker(&entry, &executable).await;
        let session_id = uuid::Uuid::new_v4().to_string();

        let (
            session,
            verification_url,
            verification_url_complete,
            expires_at_ms,
            poll_interval_ms,
            flow,
            user_code,
        ) = match method.method_type.as_str() {
            OAUTH2_ADD_METHOD => {
                let value = worker
                    .invoke(
                        "oauth.begin",
                        serde_json::json!({
                            "resourceType": resource_type,
                            "methodId": method.id,
                        }),
                        CancellationToken::new(),
                    )
                    .await?;
                let begin: OAuth2Begin = serde_json::from_value(value)?;
                (
                    begin.session,
                    begin.verification_url,
                    begin.verification_url_complete,
                    begin.expires_at_ms,
                    begin.poll_interval_ms.max(1_000),
                    OAuthFlow::DeviceCode,
                    Some(begin.user_code),
                )
            }
            OAUTH2_AUTHORIZATION_CODE_ADD_METHOD => {
                let state = oauth_random_secret();
                let code_verifier = oauth_random_secret();
                let code_challenge =
                    URL_SAFE_NO_PAD.encode(Sha256::digest(code_verifier.as_bytes()));
                let callback = method.callback.as_ref();
                let callback = oauth_callback::bind(
                    callback.and_then(|value| value.port),
                    callback
                        .and_then(|value| value.path.as_deref())
                        .unwrap_or("/oauth-callback"),
                    state.clone(),
                    entry.manifest.name.clone(),
                    entry.icon.clone(),
                    serde_json::to_value(&resource.display_name)?,
                )
                .await?;
                let redirect_uri = callback.redirect_uri.clone();
                let value = worker
                    .invoke(
                        "oauth.begin",
                        serde_json::json!({
                            "resourceType": resource_type,
                            "methodId": method.id,
                            "authorization": {
                                "redirectUri": redirect_uri,
                                "state": state,
                                "codeChallenge": code_challenge,
                            },
                        }),
                        CancellationToken::new(),
                    )
                    .await?;
                let begin: OAuth2AuthorizationCodeBegin = serde_json::from_value(value)?;
                let poll_interval_ms = begin.poll_interval_ms.unwrap_or(1_000).max(1_000);
                (
                    begin.session,
                    begin.authorization_url,
                    None,
                    begin.expires_at_ms,
                    poll_interval_ms,
                    OAuthFlow::AuthorizationCode {
                        redirect_uri,
                        code_verifier,
                        callback,
                    },
                    None,
                )
            }
            method_type => {
                return Err(Error::Config(format!(
                    "plugin '{plugin_id}' OAuth method '{method_id}' uses unsupported type '{method_type}'"
                )));
            }
        };

        if expires_at_ms <= now_ms() {
            return Err(Error::Protocol(format!(
                "plugin '{plugin_id}' OAuth method '{method_id}' returned an expired session"
            )));
        }
        self.inner.oauth_sessions.lock().await.insert(
            session_id.clone(),
            OAuthSession {
                plugin_id: plugin_id.to_owned(),
                resource_type: resource_type.to_owned(),
                method_id: method_id.to_owned(),
                session,
                expires_at_ms,
                poll_interval_ms,
                next_poll_at_ms: now_ms() + poll_interval_ms,
                flow,
            },
        );
        let cleanup = self.clone();
        let cleanup_session_id = session_id.clone();
        let cleanup_delay_ms = expires_at_ms.saturating_sub(now_ms()) as u64;
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(cleanup_delay_ms)).await;
            cleanup
                .inner
                .oauth_sessions
                .lock()
                .await
                .remove(&cleanup_session_id);
        });
        Ok(OAuthBeginResponse {
            session_id,
            user_code,
            verification_url,
            verification_url_complete,
            expires_at_ms,
            poll_interval_ms,
        })
    }

    pub async fn oauth_poll(&self, session_id: &str) -> Result<OAuthPollResponse> {
        let work = {
            let now = now_ms();
            let mut sessions = self.inner.oauth_sessions.lock().await;
            let Some(state) = sessions.get_mut(session_id) else {
                return Ok(OAuthPollResponse::Failed {
                    message: "authorization session no longer exists".into(),
                });
            };
            if now >= state.expires_at_ms {
                sessions.remove(session_id);
                return Ok(OAuthPollResponse::Failed {
                    message: "authorization expired".into(),
                });
            }
            if now < state.next_poll_at_ms {
                return Ok(OAuthPollResponse::Pending {
                    poll_interval_ms: state.poll_interval_ms,
                });
            }
            state.next_poll_at_ms = now + state.poll_interval_ms;

            let common = (
                state.plugin_id.clone(),
                state.resource_type.clone(),
                state.method_id.clone(),
                state.session.clone(),
            );
            match &mut state.flow {
                OAuthFlow::DeviceCode => OAuthPollWork::DeviceCode {
                    plugin_id: common.0,
                    resource_type: common.1,
                    method_id: common.2,
                    session: common.3,
                    poll_interval_ms: state.poll_interval_ms,
                },
                OAuthFlow::AuthorizationCode {
                    redirect_uri,
                    code_verifier,
                    callback,
                } => match callback.receiver.try_recv() {
                    Ok(callback_request) => OAuthPollWork::AuthorizationCode {
                        plugin_id: common.0,
                        resource_type: common.1,
                        method_id: common.2,
                        session: common.3,
                        redirect_uri: redirect_uri.clone(),
                        code_verifier: code_verifier.clone(),
                        callback_request,
                    },
                    Err(tokio::sync::oneshot::error::TryRecvError::Empty) => {
                        return Ok(OAuthPollResponse::Pending {
                            poll_interval_ms: state.poll_interval_ms,
                        });
                    }
                    Err(tokio::sync::oneshot::error::TryRecvError::Closed) => {
                        sessions.remove(session_id);
                        return Ok(OAuthPollResponse::Failed {
                            message: "authorization callback stopped before completion".into(),
                        });
                    }
                },
            }
        };

        match work {
            OAuthPollWork::DeviceCode {
                plugin_id,
                resource_type,
                method_id,
                session,
                poll_interval_ms,
            } => {
                self.poll_device_code(
                    session_id,
                    plugin_id,
                    resource_type,
                    method_id,
                    session,
                    poll_interval_ms,
                )
                .await
            }
            OAuthPollWork::AuthorizationCode {
                plugin_id,
                resource_type,
                method_id,
                session,
                redirect_uri,
                code_verifier,
                callback_request,
            } => {
                self.complete_authorization_code(
                    session_id,
                    plugin_id,
                    resource_type,
                    method_id,
                    session,
                    redirect_uri,
                    code_verifier,
                    callback_request,
                )
                .await
            }
        }
    }

    async fn poll_device_code(
        &self,
        session_id: &str,
        plugin_id: String,
        resource_type: String,
        method_id: String,
        session: serde_json::Value,
        poll_interval_ms: i64,
    ) -> Result<OAuthPollResponse> {
        let executable = self.executable()?;
        let entry = self.find_entry(&executable, &plugin_id).await?;
        let value = self
            .worker(&entry, &executable)
            .await
            .invoke(
                "oauth.poll",
                serde_json::json!({
                    "resourceType": resource_type,
                    "methodId": method_id,
                    "session": session,
                }),
                CancellationToken::new(),
            )
            .await?;
        let poll: OAuth2Poll = serde_json::from_value(value)?;
        match poll {
            OAuth2Poll::Pending { session } => {
                self.update_session(session_id, session, None).await;
                Ok(OAuthPollResponse::Pending { poll_interval_ms })
            }
            OAuth2Poll::SlowDown { session } => {
                let interval = poll_interval_ms + OAUTH_SLOW_DOWN_STEP_MS;
                self.update_session(session_id, session, Some(interval))
                    .await;
                Ok(OAuthPollResponse::Pending {
                    poll_interval_ms: interval,
                })
            }
            OAuth2Poll::Completed { resources } => {
                // 持久化成功后才销毁设备码会话:写盘瞬时失败时下次轮询还能重试。
                let response = self
                    .persist_oauth_resources(
                        &entry,
                        &executable,
                        &plugin_id,
                        &resource_type,
                        resources,
                    )
                    .await?;
                self.inner.oauth_sessions.lock().await.remove(session_id);
                Ok(response)
            }
            OAuth2Poll::Denied { message } => {
                self.inner.oauth_sessions.lock().await.remove(session_id);
                Ok(OAuthPollResponse::Denied { message })
            }
            OAuth2Poll::Failed { message } => {
                self.inner.oauth_sessions.lock().await.remove(session_id);
                Ok(OAuthPollResponse::Failed { message })
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn complete_authorization_code(
        &self,
        session_id: &str,
        plugin_id: String,
        resource_type: String,
        method_id: String,
        session: serde_json::Value,
        redirect_uri: String,
        code_verifier: String,
        callback_request: CallbackRequest,
    ) -> Result<OAuthPollResponse> {
        let CallbackRequest { result, response } = callback_request;
        let code = match result {
            Ok(code) => code,
            Err(message) => {
                self.inner.oauth_sessions.lock().await.remove(session_id);
                let _ = response.send(CallbackOutcome {
                    success: false,
                    message: Some(message.clone()),
                });
                return Ok(OAuthPollResponse::Denied {
                    message: Some(message),
                });
            }
        };

        let result = async {
            let executable = self.executable()?;
            let entry = self.find_entry(&executable, &plugin_id).await?;
            let value = self
                .worker(&entry, &executable)
                .await
                .invoke(
                    "oauth.complete",
                    serde_json::json!({
                        "resourceType": resource_type,
                        "methodId": method_id,
                        "session": session,
                        "authorization": {
                            "code": code,
                            "redirectUri": redirect_uri,
                            "codeVerifier": code_verifier,
                        },
                    }),
                    CancellationToken::new(),
                )
                .await?;
            let resources: Vec<ResourceDraft> = serde_json::from_value(value)?;
            self.persist_oauth_resources(&entry, &executable, &plugin_id, &resource_type, resources)
                .await
        }
        .await;

        self.inner.oauth_sessions.lock().await.remove(session_id);
        match result {
            Ok(completed) => {
                let _ = response.send(CallbackOutcome {
                    success: true,
                    message: None,
                });
                Ok(completed)
            }
            Err(error) => {
                let message = error.to_string();
                let _ = response.send(CallbackOutcome {
                    success: false,
                    message: Some(message.clone()),
                });
                Ok(OAuthPollResponse::Failed { message })
            }
        }
    }

    async fn persist_oauth_resources(
        &self,
        entry: &PluginEntry,
        executable: &Path,
        plugin_id: &str,
        resource_type: &str,
        resources: Vec<ResourceDraft>,
    ) -> Result<OAuthPollResponse> {
        let outcome = self
            .inner
            .state
            .upsert_resources(plugin_id, resource_type, resources)
            .await?;
        let model_sync_error = self
            .sync_provider_models_for_resource(entry, executable, resource_type)
            .await;
        Ok(OAuthPollResponse::Completed {
            added: outcome.added,
            updated: outcome.updated,
            model_sync_error,
        })
    }

    pub async fn import_resources(
        &self,
        plugin_id: &str,
        resource_type: &str,
        files: serde_json::Value,
    ) -> Result<ImportResponse> {
        let executable = self.executable()?;
        let entry = self.find_entry(&executable, plugin_id).await?;
        let resource = find_resource(&entry, resource_type)?;
        if resource.import.is_none() {
            return Err(Error::Config(format!(
                "plugin '{plugin_id}' resource '{resource_type}' does not support import"
            )));
        }
        let value = self
            .worker(&entry, &executable)
            .await
            .invoke(
                "import.parse",
                serde_json::json!({ "resourceType": resource_type, "files": files }),
                CancellationToken::new(),
            )
            .await?;
        let parsed: ImportParseResult = serde_json::from_value(value)?;
        if parsed.resources.is_empty() {
            return Err(Error::Config(
                parsed
                    .warnings
                    .first()
                    .cloned()
                    .unwrap_or_else(|| "import produced no resources".into()),
            ));
        }
        if parsed.resources.len() > MAX_IMPORT_DRAFTS {
            return Err(Error::Config(format!(
                "import produced more than {MAX_IMPORT_DRAFTS} resources"
            )));
        }
        let outcome = self
            .inner
            .state
            .upsert_resources(plugin_id, resource_type, parsed.resources)
            .await?;
        let model_sync_error = self
            .sync_provider_models_for_resource(&entry, &executable, resource_type)
            .await;
        Ok(ImportResponse {
            added: outcome.added,
            updated: outcome.updated,
            warnings: parsed.warnings,
            model_sync_error,
        })
    }

    /// 导出某资源类型的全部私有数据,供备份或迁移;格式与批量导入兼容。
    pub async fn export_resources(
        &self,
        plugin_id: &str,
        resource_type: &str,
    ) -> Result<serde_json::Value> {
        let executable = self.executable()?;
        let entry = self.find_entry(&executable, plugin_id).await?;
        find_resource(&entry, resource_type)?;
        let records = self.inner.state.resources(plugin_id, resource_type).await?;
        Ok(serde_json::json!({
            "accounts": records
                .iter()
                .map(|record| record.private_data.clone())
                .collect::<Vec<_>>(),
        }))
    }

    pub async fn refresh_resource(
        &self,
        plugin_id: &str,
        resource_type: &str,
        resource_id: &str,
    ) -> Result<()> {
        let executable = self.executable()?;
        let entry = self.find_entry(&executable, plugin_id).await?;
        let resource = find_resource(&entry, resource_type)?;
        if !resource.can_refresh {
            return Err(Error::Config(format!(
                "plugin '{plugin_id}' resource '{resource_type}' does not support refresh"
            )));
        }
        let record = self
            .find_record(plugin_id, resource_type, resource_id)
            .await?;
        self.refresh_record(&entry, &executable, resource_type, &record)
            .await
    }

    /// 启动后台额度刷新循环:每分钟刷新一次全部可刷新资源并把聚合额度块
    /// 写入缓存,供 Cursor 模型目录追加到插件模型的 hover 备注末尾。
    /// interval 首 tick 立即执行;运行时未就绪的 tick 直接跳过。
    pub fn start_quota_refresh(&self) {
        let registry = self.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(QUOTA_REFRESH_INTERVAL);
            loop {
                interval.tick().await;
                registry.refresh_quota_summaries().await;
            }
        });
    }

    /// Cursor 模型目录消费的额度块;provider 无资源类型或无额度数据时为 None。
    pub async fn quota_summary(&self, plugin_id: &str, provider_id: &str) -> Option<String> {
        let executable = self.inner.runtime.executable()?;
        let entry = self.find_entry(&executable, plugin_id).await.ok()?;
        let resource_type = find_provider(&entry, provider_id)
            .ok()?
            .resource_type
            .as_deref()?;
        let key = quota_key(plugin_id, resource_type);
        self.inner.quota_summaries.read().await.get(&key).cloned()
    }

    /// 某 Provider 全部模型的动态备注(随额度同轮刷新);无备注能力的 provider 返回 None。
    /// 按键 `{plugin_id}/{provider_id}` 一次取整张表,避免逐模型重复取锁。
    pub async fn model_notes(
        &self,
        plugin_id: &str,
        provider_id: &str,
    ) -> Option<HashMap<String, String>> {
        let key = notes_key(plugin_id, provider_id);
        self.inner.model_notes.read().await.get(&key).cloned()
    }

    /// 一轮额度刷新:对每个可刷新资源类型,先逐账号调用插件 refresh 落盘,
    /// 再用 present 投影聚合成额度块;整轮结果整体替换缓存。
    async fn refresh_quota_summaries(&self) {
        let Some(executable) = self.inner.runtime.executable() else {
            return;
        };
        let disabled = self
            .inner
            .store
            .disabled_plugin_accounts()
            .await
            .unwrap_or_default();
        let entries = self.entries(&executable).await;
        let mut summaries = HashMap::new();
        for entry in &entries {
            let plugin_id = entry.manifest.id.clone();
            for definition in &entry.definition.resources {
                if !definition.can_refresh {
                    continue;
                }
                let resource_type = definition.resource_type.as_str();
                let records = match self.inner.state.resources(&plugin_id, resource_type).await {
                    Ok(records) => records,
                    Err(error) => {
                        tracing::warn!(plugin = %plugin_id, %error, "cannot load plugin resources for quota refresh");
                        continue;
                    }
                };
                let enabled: Vec<ResourceRecord> = records
                    .into_iter()
                    .filter(|record| !disabled.contains(&record.id))
                    .collect();
                if enabled.is_empty() {
                    continue;
                }
                for record in &enabled {
                    if let Err(error) = self
                        .refresh_record(entry, &executable, resource_type, record)
                        .await
                    {
                        tracing::warn!(plugin = %plugin_id, account = %record.key, %error, "plugin quota refresh failed");
                    }
                }
                // 重新读取以拿到 refresh 写入的最新 private_data。
                let records = self
                    .inner
                    .state
                    .resources(&plugin_id, resource_type)
                    .await
                    .unwrap_or_else(|_| enabled.clone());
                let enabled: Vec<ResourceRecord> = records
                    .into_iter()
                    .filter(|record| !disabled.contains(&record.id))
                    .collect();
                let views = self
                    .present_resources(entry, &executable, definition, &enabled)
                    .await;
                if let Some(block) = quota::quota_block(&views) {
                    summaries.insert(quota_key(&plugin_id, resource_type), block);
                }
            }
        }
        *self.inner.quota_summaries.write().await = summaries;
        self.refresh_model_notes(&entries).await;
    }

    /// 与额度摘要同一轮刷新模型备注:对每个声明了 notes 能力的 provider,
    /// 用已持久化的模型快照调用插件 notes,结果按模型 ID 缓存;整轮整体替换。
    async fn refresh_model_notes(&self, entries: &[PluginEntry]) {
        let Some(executable) = self.inner.runtime.executable() else {
            return;
        };
        let mut all_notes = HashMap::new();
        for entry in entries {
            let plugin_id = entry.manifest.id.clone();
            for provider in &entry.definition.providers {
                if !provider.has_notes {
                    continue;
                }
                // 把已持久化的模型快照一并传给插件,插件据此派生备注,无需再拉上游目录。
                let models = self
                    .inner
                    .state
                    .models(&plugin_id, &provider.id)
                    .await
                    .unwrap_or_default()
                    .iter()
                    .map(StoredModel::snapshot)
                    .collect::<Vec<_>>();
                let value = self
                    .worker(entry, &executable)
                    .await
                    .invoke(
                        "models.notes",
                        serde_json::json!({
                            "providerId": provider.id,
                            "models": models,
                        }),
                        CancellationToken::new(),
                    )
                    .await;
                let notes = match value {
                    Ok(value) => match serde_json::from_value::<Vec<ModelNote>>(value) {
                        Ok(notes) => notes,
                        Err(error) => {
                            tracing::warn!(plugin = %plugin_id, provider = %provider.id, %error, "plugin model notes parse failed");
                            Vec::new()
                        }
                    },
                    Err(error) => {
                        tracing::warn!(plugin = %plugin_id, provider = %provider.id, %error, "plugin model notes refresh failed");
                        Vec::new()
                    }
                };
                if notes.is_empty() {
                    continue;
                }
                let by_model: HashMap<String, String> = notes
                    .into_iter()
                    .map(|note| (note.model_id, quota::zh_text(&note.text)))
                    .collect();
                all_notes.insert(notes_key(&plugin_id, &provider.id), by_model);
            }
        }
        *self.inner.model_notes.write().await = all_notes;
    }

    /// 调用插件刷新单条资源并把返回的 patch 落盘。
    async fn refresh_record(
        &self,
        entry: &PluginEntry,
        executable: &Path,
        resource_type: &str,
        record: &ResourceRecord,
    ) -> Result<()> {
        let value = self
            .worker(entry, executable)
            .await
            .invoke(
                "resource.refresh",
                serde_json::json!({
                    "resourceType": resource_type,
                    "resource": record.snapshot(resource_type),
                }),
                CancellationToken::new(),
            )
            .await?;
        let patch: ResourcePatch = serde_json::from_value(value)?;
        self.inner
            .state
            .apply_patch(&entry.manifest.id, resource_type, &record.id, patch)
            .await
    }

    pub async fn resource_action(
        &self,
        plugin_id: &str,
        resource_type: &str,
        resource_id: &str,
        action_id: &str,
        input: serde_json::Value,
    ) -> Result<serde_json::Value> {
        let executable = self.executable()?;
        let entry = self.find_entry(&executable, plugin_id).await?;
        let resource = find_resource(&entry, resource_type)?;
        let action = resource
            .actions
            .iter()
            .find(|action| action.id == action_id)
            .ok_or_else(|| {
                Error::Config(format!(
                    "plugin '{plugin_id}' resource '{resource_type}' does not define action '{action_id}'"
                ))
            })?;
        if !matches!(action.target.as_str(), "resource" | "card") {
            return Err(Error::Config(format!(
                "plugin '{plugin_id}' resource action '{action_id}' has an invalid target"
            )));
        }
        let record = self
            .find_record(plugin_id, resource_type, resource_id)
            .await?;
        let value = self
            .worker(&entry, &executable)
            .await
            .invoke(
                "resource.action",
                serde_json::json!({
                    "resourceType": resource_type,
                    "actionId": action_id,
                    "resource": record.snapshot(resource_type),
                    "input": input,
                }),
                CancellationToken::new(),
            )
            .await?;
        let result: ResourceActionResult = serde_json::from_value(value)?;
        if let Some(patch) = result.patch.clone() {
            self.inner
                .state
                .apply_patch(plugin_id, resource_type, resource_id, patch)
                .await?;
        }
        Ok(serde_json::to_value(ResourceActionResponse::from(result))?)
    }

    pub async fn delete_resource(
        &self,
        plugin_id: &str,
        resource_type: &str,
        resource_id: &str,
    ) -> Result<()> {
        let executable = self.executable()?;
        let entry = self.find_entry(&executable, plugin_id).await?;
        let resource = find_resource(&entry, resource_type)?;
        let record = self
            .find_record(plugin_id, resource_type, resource_id)
            .await?;
        if resource.can_remove {
            // 上游撤销失败不阻塞本地删除:用户必须能移除已失效的资源。
            if let Err(error) = self
                .worker(&entry, &executable)
                .await
                .invoke(
                    "resource.remove",
                    serde_json::json!({
                        "resourceType": resource_type,
                        "resource": record.snapshot(resource_type),
                    }),
                    CancellationToken::new(),
                )
                .await
            {
                tracing::warn!(plugin = %plugin_id, %error, "plugin resource remove hook failed");
            }
        }
        self.inner
            .state
            .remove_resource(plugin_id, resource_type, resource_id)
            .await?;
        Ok(())
    }

    pub async fn sync_models(&self, plugin_id: &str, provider_id: &str) -> Result<usize> {
        let executable = self.executable()?;
        let entry = self.find_entry(&executable, plugin_id).await?;
        let provider = find_provider(&entry, provider_id)?.clone();
        self.sync_provider_models(&entry, &executable, &provider)
            .await
    }

    /// 清空插件在本机持久化的全部数据:账号资源、模型目录,以及数据库中的
    /// 模型开关、模型覆盖与账号停用标记。插件保持安装,等同于刚安装、
    /// 尚未添加账号的状态;重新添加账号后会重新同步模型。
    pub async fn clear_data(&self, plugin_id: &str) -> Result<()> {
        let executable = self.executable()?;
        let entry = self.find_entry(&executable, plugin_id).await?;
        // 停用账号按资源记录 ID 保存,必须在数据目录删除前读出。
        let mut account_ids = Vec::new();
        for resource in &entry.definition.resources {
            account_ids.extend(
                self.inner
                    .state
                    .resources(plugin_id, &resource.resource_type)
                    .await?
                    .into_iter()
                    .map(|record| record.id),
            );
        }
        // 统一 ID 不含插件前缀:按该插件当前已知的模型 ID 集合清理设置。
        let mut model_ids = Vec::new();
        for provider in &entry.definition.providers {
            for model in self
                .inner
                .state
                .models(plugin_id, &provider.id)
                .await
                .unwrap_or_default()
            {
                model_ids.push(crate::model::plugin_model_id(plugin_id, &model.id));
            }
        }
        if let Some(worker) = self.inner.workers.lock().await.remove(plugin_id) {
            worker.stop().await;
        }
        self.inner
            .store
            .clear_plugin_settings(&model_ids, &account_ids)
            .await?;
        self.inner.state.clear(plugin_id).await
    }

    async fn descriptor(&self, entry: &PluginEntry, executable: &Path) -> PluginDescriptor {
        let plugin_id = &entry.manifest.id;
        let disabled_models = self
            .inner
            .store
            .disabled_plugin_models()
            .await
            .unwrap_or_default();
        let overrides = self
            .inner
            .store
            .plugin_model_overrides()
            .await
            .unwrap_or_default();
        let disabled_accounts = self
            .inner
            .store
            .disabled_plugin_accounts()
            .await
            .unwrap_or_default();
        let mut providers = Vec::new();
        for provider in &entry.definition.providers {
            let stored = self
                .inner
                .state
                .models(plugin_id, &provider.id)
                .await
                .unwrap_or_default();
            let configured = self
                .provider_configured(plugin_id, provider, &stored, &disabled_accounts)
                .await;
            providers.push(PluginProviderDescriptor {
                id: provider.id.clone(),
                plugin_id: plugin_id.clone(),
                display_name: provider.display_name.clone(),
                description: provider.description.clone(),
                provider_type: provider.provider_type.clone(),
                resource_type: provider.resource_type.clone(),
                has_models: provider.has_models,
                configured,
                models: stored
                    .iter()
                    .map(|model| {
                        let descriptor = PluginModelDescriptor::new(
                            plugin_id,
                            &entry.manifest.name,
                            &entry.icon,
                            provider,
                            model,
                        );
                        let mut descriptor = match overrides.get(&descriptor.id) {
                            Some(over) => descriptor.with_override(over),
                            None => descriptor,
                        };
                        descriptor.enabled = !disabled_models.contains(&descriptor.id);
                        descriptor
                    })
                    .collect(),
            });
        }
        let mut resources = Vec::new();
        for definition in &entry.definition.resources {
            let records = self
                .inner
                .state
                .resources(plugin_id, &definition.resource_type)
                .await
                .unwrap_or_default();
            let views = self
                .present_resources(entry, executable, definition, &records)
                .await;
            resources.push(PluginResourceDescriptor {
                resource_type: definition.resource_type.clone(),
                display_name: definition.display_name.clone(),
                add: definition.add.clone(),
                import: definition.import.clone(),
                actions: definition.actions.clone(),
                can_refresh: definition.can_refresh,
                can_remove: definition.can_remove,
                resources: views,
            });
        }
        PluginDescriptor {
            id: plugin_id.clone(),
            name: entry.manifest.name.clone(),
            version: entry.manifest.version.clone(),
            author: entry.manifest.author.clone(),
            icon: entry.icon.clone(),
            providers,
            resources,
        }
    }

    async fn present_resources(
        &self,
        entry: &PluginEntry,
        executable: &Path,
        definition: &ResourceDefinition,
        records: &[ResourceRecord],
    ) -> Vec<PluginResourceView> {
        if records.is_empty() {
            return Vec::new();
        }
        let snapshots = records
            .iter()
            .map(|record| record.snapshot(&definition.resource_type))
            .collect::<Vec<_>>();
        let presented = self
            .worker(entry, executable)
            .await
            .invoke(
                "resource.present",
                serde_json::json!({
                    "resourceType": definition.resource_type,
                    "resources": snapshots,
                }),
                CancellationToken::new(),
            )
            .await
            .and_then(|value| {
                serde_json::from_value::<Vec<ResourcePresentation>>(value).map_err(Error::from)
            });
        match presented {
            Ok(views) if views.len() == records.len() => records
                .iter()
                .zip(views)
                .map(|(record, view)| PluginResourceView::from_record(record, view))
                .collect(),
            Ok(_) | Err(_) => records
                .iter()
                .map(|record| {
                    PluginResourceView::from_record(
                        record,
                        ResourcePresentation {
                            display_name: record.key.clone(),
                            tier: None,
                            metrics: Vec::new(),
                        },
                    )
                })
                .collect(),
        }
    }

    async fn provider_configured(
        &self,
        plugin_id: &str,
        provider: &ProviderDefinition,
        stored_models: &[StoredModel],
        disabled_accounts: &std::collections::HashSet<String>,
    ) -> bool {
        if provider.has_models && stored_models.is_empty() {
            return false;
        }
        match &provider.resource_type {
            Some(resource_type) => {
                let resources = self
                    .inner
                    .state
                    .resources(plugin_id, resource_type)
                    .await
                    .unwrap_or_default();
                resources
                    .iter()
                    .any(|record| !disabled_accounts.contains(&record.id))
            }
            None => true,
        }
    }

    /// 资源到位后刷新使用该资源类型的 Provider 模型目录;失败只报告不中断。
    async fn sync_provider_models_for_resource(
        &self,
        entry: &PluginEntry,
        executable: &Path,
        resource_type: &str,
    ) -> Option<String> {
        let mut errors = Vec::new();
        for provider in entry.definition.providers.clone() {
            if provider.resource_type.as_deref() != Some(resource_type) || !provider.has_models {
                continue;
            }
            if let Err(error) = self
                .sync_provider_models(entry, executable, &provider)
                .await
            {
                errors.push(format!("{}: {error}", provider.id));
            }
        }
        (!errors.is_empty()).then(|| errors.join("; "))
    }

    async fn sync_provider_models(
        &self,
        entry: &PluginEntry,
        executable: &Path,
        provider: &ProviderDefinition,
    ) -> Result<usize> {
        if !provider.has_models {
            return Err(Error::Config(format!(
                "plugin provider '{}' does not enumerate models",
                provider.id
            )));
        }
        let plugin_id = &entry.manifest.id;
        let resource = match &provider.resource_type {
            Some(resource_type) => {
                let record = self.select_resource(plugin_id, resource_type).await?;
                Some(record.snapshot(resource_type))
            }
            None => None,
        };
        let value = self
            .worker(entry, executable)
            .await
            .invoke(
                "models.list",
                serde_json::json!({ "providerId": provider.id, "resource": resource }),
                CancellationToken::new(),
            )
            .await?;
        let definitions = value
            .as_array()
            .ok_or_else(|| Error::Protocol("plugin models.list must return an array".into()))?;
        let mut models = Vec::with_capacity(definitions.len());
        let mut seen = std::collections::HashSet::new();
        for definition in definitions {
            let model = StoredModel::from_definition(definition)?;
            if !seen.insert(model.id.clone()) {
                return Err(Error::Config(format!(
                    "duplicate plugin upstream model ID: '{}'",
                    model.id
                )));
            }
            models.push(model);
        }
        if models.is_empty() {
            return Err(Error::Provider(format!(
                "plugin provider '{}' returned no models",
                provider.id
            )));
        }
        let _write = self.lock_model_writes().await;
        // Replacement, not first-match lookup: provider ID remains routing metadata.
        let mut inventory = self.all_models().await?;
        inventory.retain(|model| model.plugin_id != *plugin_id || model.provider_id != provider.id);
        inventory.extend(models.iter().map(|model| {
            PluginModelDescriptor::new(
                plugin_id,
                &entry.manifest.name,
                &entry.icon,
                provider,
                model,
            )
        }));
        crate::model::ModelDirectory::new(&self.inner.store.models().await?, &inventory)?;
        self.inner
            .state
            .replace_models(plugin_id, &provider.id, &models)
            .await?;
        Ok(models.len())
    }

    /// 候选账号按套餐优先级排序后稳定排列;带亲和键(会话 ID)时以哈希
    /// 决定首选,同一会话在候选集不变期间稳定落在同一账号,冷却或停用
    /// 的账号被排除后自然漂移。返回顺序即同一次请求故障转移的尝试顺序。
    async fn select_resources(
        &self,
        plugin_id: &str,
        resource_type: &str,
        affinity: Option<&str>,
    ) -> Result<Vec<ResourceRecord>> {
        let disabled_accounts = self
            .inner
            .store
            .disabled_plugin_accounts()
            .await
            .unwrap_or_default();
        let records = self.inner.state.resources(plugin_id, resource_type).await?;
        let active_records: Vec<_> = records
            .into_iter()
            .filter(|record| !disabled_accounts.contains(&record.id))
            .collect();
        if active_records.is_empty() {
            return Err(Error::Provider(format!(
                "plugin '{plugin_id}' has no enabled '{resource_type}' resource; enable or add one first"
            )));
        }
        let mut ready_records =
            selection::order_candidates(&active_records, super::state::now_ms());
        if ready_records.is_empty() {
            return Err(Error::Provider(format!(
                "all enabled accounts for plugin '{plugin_id}' are currently cooling or rate-limited"
            )));
        }
        let turn = if affinity.is_none() {
            self.inner
                .rr_counter
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        } else {
            0
        };
        selection::rotate_candidates(&mut ready_records, affinity, turn);
        Ok(ready_records)
    }

    async fn select_resource(
        &self,
        plugin_id: &str,
        resource_type: &str,
    ) -> Result<ResourceRecord> {
        let mut candidates = self
            .select_resources(plugin_id, resource_type, None)
            .await?;
        Ok(candidates.remove(0))
    }

    async fn find_record(
        &self,
        plugin_id: &str,
        resource_type: &str,
        resource_id: &str,
    ) -> Result<ResourceRecord> {
        self.inner
            .state
            .resources(plugin_id, resource_type)
            .await?
            .into_iter()
            .find(|record| record.id == resource_id)
            .ok_or_else(|| Error::RunNotFound(format!("plugin resource {resource_id}")))
    }

    /// 仅测试用:直接写入资源记录,绕过插件工作进程;返回 key → id 映射。
    #[cfg(test)]
    async fn seed_resources(
        &self,
        plugin_id: &str,
        resource_type: &str,
        drafts: Vec<ResourceDraft>,
    ) -> HashMap<String, String> {
        self.inner
            .state
            .upsert_resources(plugin_id, resource_type, drafts)
            .await
            .unwrap();
        self.inner
            .state
            .resources(plugin_id, resource_type)
            .await
            .unwrap()
            .into_iter()
            .map(|record| (record.key, record.id))
            .collect()
    }

    /// 仅测试用:把账号置入指定时长的冷却。
    #[cfg(test)]
    async fn cool_account(
        &self,
        plugin_id: &str,
        resource_type: &str,
        resource_id: &str,
        retry_in_ms: i64,
    ) {
        let patch = ResourcePatch {
            private_data: None,
            state: Some(crate::plugin::state::ResourceStateInput::Cooling {
                retry_at_ms: Some(now_ms() + retry_in_ms),
                message: None,
            }),
        };
        self.inner
            .state
            .apply_patch(plugin_id, resource_type, resource_id, patch)
            .await
            .unwrap();
    }

    async fn update_session(
        &self,
        session_id: &str,
        session: Option<serde_json::Value>,
        poll_interval_ms: Option<i64>,
    ) {
        let mut sessions = self.inner.oauth_sessions.lock().await;
        if let Some(state) = sessions.get_mut(session_id) {
            if let Some(session) = session {
                state.session = session;
            }
            if let Some(interval) = poll_interval_ms {
                state.poll_interval_ms = interval;
            }
        }
    }

    fn executable(&self) -> Result<std::path::PathBuf> {
        self.inner
            .runtime
            .executable()
            .ok_or_else(|| Error::Config("plugin runtime is not ready".into()))
    }

    async fn entries(&self, executable: &Path) -> Vec<PluginEntry> {
        if let Some(entries) = self.inner.entries.read().await.as_ref() {
            return entries.clone();
        }
        let loaded = self.inner.catalog.entries(executable).await;
        *self.inner.entries.write().await = Some(loaded.clone());
        loaded
    }

    async fn find_entry(&self, executable: &Path, plugin_id: &str) -> Result<PluginEntry> {
        self.entries(executable)
            .await
            .into_iter()
            .find(|entry| entry.manifest.id == plugin_id)
            .ok_or_else(|| Error::RunNotFound(format!("plugin {plugin_id}")))
    }

    async fn worker(&self, entry: &PluginEntry, executable: &Path) -> Arc<PluginWorker> {
        let mut workers = self.inner.workers.lock().await;
        workers
            .entry(entry.manifest.id.clone())
            .or_insert_with(|| {
                Arc::new(PluginWorker::new(
                    entry,
                    executable.to_path_buf(),
                    self.inner.catalog.loader().clone(),
                    self.inner.store.clone(),
                ))
            })
            .clone()
    }
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct OAuth2Begin {
    session: serde_json::Value,
    user_code: String,
    verification_url: String,
    #[serde(default)]
    verification_url_complete: Option<String>,
    expires_at_ms: i64,
    poll_interval_ms: i64,
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct OAuth2AuthorizationCodeBegin {
    session: serde_json::Value,
    authorization_url: String,
    expires_at_ms: i64,
    #[serde(default)]
    poll_interval_ms: Option<i64>,
}

fn oauth_random_secret() -> String {
    let first = uuid::Uuid::new_v4();
    let second = uuid::Uuid::new_v4();
    let mut bytes = [0_u8; 32];
    bytes[..16].copy_from_slice(first.as_bytes());
    bytes[16..].copy_from_slice(second.as_bytes());
    URL_SAFE_NO_PAD.encode(bytes)
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "kebab-case", tag = "status")]
enum OAuth2Poll {
    #[serde(rename_all = "camelCase")]
    Pending {
        #[serde(default)]
        session: Option<serde_json::Value>,
    },
    #[serde(rename_all = "camelCase")]
    SlowDown {
        #[serde(default)]
        session: Option<serde_json::Value>,
    },
    #[serde(rename_all = "camelCase")]
    Completed { resources: Vec<ResourceDraft> },
    #[serde(rename_all = "camelCase")]
    Denied {
        #[serde(default)]
        message: Option<String>,
    },
    #[serde(rename_all = "camelCase")]
    Failed { message: String },
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ImportParseResult {
    resources: Vec<ResourceDraft>,
    #[serde(default)]
    warnings: Vec<String>,
}

/// 额度摘要缓存键。
fn quota_key(plugin_id: &str, resource_type: &str) -> String {
    format!("{plugin_id}/{resource_type}")
}

/// 模型备注缓存键。
fn notes_key(plugin_id: &str, provider_id: &str) -> String {
    format!("{plugin_id}/{provider_id}")
}

fn find_provider<'a>(entry: &'a PluginEntry, provider_id: &str) -> Result<&'a ProviderDefinition> {
    entry
        .definition
        .providers
        .iter()
        .find(|provider| provider.id == provider_id)
        .ok_or_else(|| {
            Error::RunNotFound(format!(
                "plugin '{}' provider {provider_id}",
                entry.manifest.id
            ))
        })
}

fn find_resource<'a>(
    entry: &'a PluginEntry,
    resource_type: &str,
) -> Result<&'a ResourceDefinition> {
    entry
        .definition
        .resources
        .iter()
        .find(|resource| resource.resource_type == resource_type)
        .ok_or_else(|| {
            Error::RunNotFound(format!(
                "plugin '{}' resource type {resource_type}",
                entry.manifest.id
            ))
        })
}

// 测试辅助函数,置于 tests 模块之前以避免 items-after-test-module 告警。
#[cfg(test)]
fn ids(records: &[ResourceRecord]) -> Vec<&str> {
    records.iter().map(|record| record.id.as_str()).collect()
}

#[cfg(test)]
fn pro_draft(id: &str) -> ResourceDraft {
    ResourceDraft {
        key: id.to_owned(),
        private_data: serde_json::json!({"quota": {"planLabel": "Pro"}}),
        state: None,
    }
}

#[cfg(test)]
fn free_draft(id: &str) -> ResourceDraft {
    ResourceDraft {
        key: id.to_owned(),
        private_data: serde_json::json!({}),
        state: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::StreamExt;

    async fn test_registry() -> PluginRegistry {
        let directory = tempfile::tempdir().unwrap();
        let store = crate::store::Store::connect(&format!(
            "sqlite://{}",
            directory.path().join("plugin-selection.db").display()
        ))
        .await
        .unwrap();
        PluginRegistry {
            inner: Arc::new(RegistryInner {
                store,
                runtime: PluginRuntime::managed().unwrap(),
                catalog: PluginCatalog::managed("test".into()).unwrap(),
                state: PluginStateStore::new(
                    crate::plugin::data::PluginDataStore::for_test(
                        directory.path().join("plugins/data"),
                    )
                    .unwrap(),
                ),
                entries: RwLock::new(None),
                workers: Mutex::new(HashMap::new()),
                oauth_sessions: Mutex::new(HashMap::new()),
                quota_summaries: RwLock::new(HashMap::new()),
                model_notes: RwLock::new(HashMap::new()),
                model_writes: Mutex::new(()),
                rr_counter: std::sync::atomic::AtomicUsize::new(0),
            }),
        }
    }

    /// 就绪优先级 + 亲和键共同决定候选顺序:Pro 排在 Free 前,
    /// 同级按 ID 决胜;亲和键哈希只决定旋转起点,不改变相对顺序。
    #[tokio::test]
    async fn select_resources_orders_by_plan_and_rotates_for_affinity() {
        let registry = test_registry().await;
        let by_key = registry
            .seed_resources(
                "com.test",
                "account",
                vec![free_draft("f1"), pro_draft("p1"), free_draft("f2")],
            )
            .await;
        // 同级按 ID 决胜:Pro 在前,Free 按 ID 升序。
        let mut free_keys = ["f1", "f2"];
        free_keys.sort_by_key(|key| by_key[*key].clone());
        let expected: Vec<&str> = std::iter::once("p1")
            .chain(free_keys)
            .map(|key| by_key[key].as_str())
            .collect();
        let no_affinity = registry
            .select_resources("com.test", "account", None)
            .await
            .unwrap();
        assert_eq!(ids(&no_affinity), expected, "套餐优先级在前,ID 决胜");
        let rotated = registry
            .select_resources("com.test", "account", Some("session-a"))
            .await
            .unwrap();
        let start = selection::affinity_index("session-a", 3);
        let expected_rotation: Vec<&str> = expected
            .iter()
            .cycle()
            .skip(start)
            .take(3)
            .copied()
            .collect();
        assert_eq!(
            ids(&rotated),
            expected_rotation,
            "亲和只旋转,不改变相对顺序"
        );
    }

    /// 停用账号与冷却中账号被过滤;全部冷却时报错。
    #[tokio::test]
    async fn select_resources_excludes_disabled_and_cooling_accounts() {
        let registry = test_registry().await;
        let by_key = registry
            .seed_resources(
                "com.test",
                "account",
                vec![free_draft("keep"), pro_draft("off"), pro_draft("cool")],
            )
            .await;
        registry
            .inner
            .store
            .set_disabled_plugin_accounts(&std::collections::HashSet::from([by_key["off"].clone()]))
            .await
            .unwrap();
        registry
            .cool_account("com.test", "account", &by_key["cool"], 86_400_000)
            .await;
        let candidates = registry
            .select_resources("com.test", "account", None)
            .await
            .unwrap();
        assert_eq!(
            ids(&candidates),
            [by_key["keep"].as_str()],
            "停用与冷却中的账号被排除"
        );
        registry
            .cool_account("com.test", "account", &by_key["keep"], 86_400_000)
            .await;
        let error = registry
            .select_resources("com.test", "account", None)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("cooling or rate-limited"));
    }

    #[tokio::test]
    async fn worker_resource_errors_fail_over_only_before_output() {
        let executable = std::path::PathBuf::from("deno");
        if std::process::Command::new(&executable)
            .arg("--version")
            .output()
            .is_err()
        {
            eprintln!("Deno unavailable: skipping worker failover integration test");
            return;
        }
        for after_output in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let entry_path = directory.path().join("main.ts");
            std::fs::write(&entry_path, include_str!("fixtures/failover.ts")).unwrap();
            let loader =
                super::super::definition::PluginDefinitionLoader::for_test(directory.path())
                    .unwrap();
            let definition = loader
                .load(&executable, directory.path(), &entry_path)
                .await
                .unwrap();
            let entry = PluginEntry {
                directory: directory.path().to_owned(), entry: entry_path,
                manifest: serde_json::from_value(serde_json::json!({
                    "apiVersion":1, "id":"com.test", "name":"Test", "version":"1.0.0", "icon":"unused.svg", "entry":"main.ts"
                })).unwrap(), definition, icon: String::new(),
            };
            let mut registry = test_registry().await;
            Arc::get_mut(&mut registry.inner).unwrap().runtime =
                PluginRuntime::for_test(directory.path().join("runtime"), executable.clone())
                    .unwrap();
            *registry.inner.entries.write().await = Some(vec![entry.clone()]);
            let worker = Arc::new(PluginWorker::new(
                &entry,
                executable.clone(),
                loader,
                registry.inner.store.clone(),
            ));
            registry
                .inner
                .workers
                .lock()
                .await
                .insert("com.test".into(), worker.clone());
            registry
                .seed_resources(
                    "com.test",
                    "account",
                    vec![free_draft("a"), free_draft("b")],
                )
                .await;
            let stored: super::super::state::StoredModel = serde_json::from_value(serde_json::json!({
                "id": if after_output { "after-output" } else { "before-output" }, "display_name":"Fixture"
            })).unwrap();
            registry
                .inner
                .state
                .replace_models("com.test", "test", std::slice::from_ref(&stored))
                .await
                .unwrap();
            let descriptor = PluginModelDescriptor::new(
                "com.test",
                "Test",
                "",
                &entry.definition.providers[0],
                &stored,
            );
            let call_id = format!("failover-{after_output}");
            let recorder = CallRecorder::start(
                registry.inner.store.clone(),
                crate::model::NewLlmCall {
                    call_id: call_id.clone(),
                    run_id: "run".into(),
                    conversation_id: "conversation".into(),
                    provider_call_index: 0,
                    model_hash: descriptor.id.clone(),
                    provider_type: crate::model::ProviderType::Plugin,
                    provider_url: "plugin://com.test/test".into(),
                    request_type: crate::model::ProviderType::Plugin,
                    request_url: "plugin://com.test/test".into(),
                    model_id: stored.id.clone(),
                    display_name: "Fixture".into(),
                    reasoning_effort: None,
                    fast: false,
                    message_count: 0,
                    tool_count: 0,
                    detailed: false,
                },
            )
            .await
            .unwrap();
            let invocation = ModelInvocation {
                call_id: call_id.clone(),
                run_id: "run".into(),
                conversation_id: "conversation".into(),
                provider_call_index: 0,
                request: crate::model::ModelRequest {
                    model: crate::model::ModelSpec::new(&descriptor.id),
                    prompt: crate::model::PromptSpec {
                        instructions: String::new(),
                        tools: Vec::new(),
                    },
                    history: Vec::new(),
                },
            };
            let events = tokio::time::timeout(
                std::time::Duration::from_secs(15),
                registry
                    .stream_model(descriptor, invocation, CancellationToken::new(), recorder)
                    .collect::<Vec<_>>(),
            )
            .await
            .unwrap();
            let text: String = events
                .iter()
                .filter_map(|event| match event {
                    Ok(ModelEvent::TextDelta(text)) => Some(text.as_str()),
                    _ => None,
                })
                .collect();
            if after_output {
                assert_eq!(text, "attempt:1");
                assert!(events.last().unwrap().is_err());
            } else {
                assert_eq!(text, "attempt:2");
                assert!(events.iter().all(Result::is_ok));
            }
            let resources = registry
                .inner
                .state
                .resources("com.test", "account")
                .await
                .unwrap();
            assert_eq!(
                resources
                    .iter()
                    .filter(|record| matches!(
                        record.state,
                        super::super::state::ResourceState::Invalid { .. }
                    ))
                    .count(),
                1
            );
            worker.stop().await;
        }
    }

    /// 失败切换序列由 can_failover 驱动,与 stream_model 的重试循环一致:
    /// 未输出且仍有候选才切换;已输出或候选耗尽则终止。
    #[test]
    fn failover_sequence_matches_stream_model_retry_loop() {
        let attempts = 3;
        let mut attempt = 0;
        let mut tried = Vec::new();
        let mut emitted = false;
        loop {
            tried.push(attempt);
            // 首个候选模拟未输出即失败,第二个模拟已输出后失败。
            if attempt == 1 {
                emitted = true;
            }
            if !selection::can_failover(emitted, attempt, attempts) {
                break;
            }
            attempt += 1;
        }
        assert_eq!(tried, [0, 1]);
    }
}
