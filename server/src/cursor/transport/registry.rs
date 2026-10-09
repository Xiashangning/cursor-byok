//! Maps request IDs to active transport handles.

use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
};

use tokio::sync::{mpsc, Mutex, Notify};

use crate::{
    cursor::{
        conversation::ConversationRegistry, prompting::PromptCompiler,
        services::observability::CursorTraceService,
    },
    plugin::PluginRegistry,
    provider::Provider,
    search::WebCache,
    store::Store,
    Result,
};

use super::{OutputHub, TransportHandle};

#[derive(Clone)]
pub struct TransportRegistry {
    inner: Arc<RegistryInner>,
}

struct RegistryInner {
    local: Mutex<HashMap<String, LocalTransport>>,
    next_local_generation: AtomicU64,
    upstream: Mutex<HashMap<String, u64>>,
    upstream_tasks: Mutex<HashMap<String, OfficialTask>>,
    next_upstream_generation: AtomicU64,
    route_changed: Notify,
    store: Store,
    traces: CursorTraceService,
    web_cache: WebCache,
    plugins: Option<PluginRegistry>,
    conversations: ConversationRegistry,
}

struct OfficialTask {
    generation: u64,
    parent_request_id: String,
    owner: u64,
    exec_id: u32,
    model: String,
}

#[derive(Clone)]
struct LocalTransport {
    generation: u64,
    handle: TransportHandle,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransportRoute {
    Local,
    Upstream(u64),
}

impl TransportRegistry {
    pub fn new(store: Store, provider: Arc<dyn Provider>, compiler: PromptCompiler) -> Self {
        Self::with_web_cache(store, provider, compiler, WebCache::default())
    }

    pub fn with_web_cache(
        store: Store,
        provider: Arc<dyn Provider>,
        compiler: PromptCompiler,
        web_cache: WebCache,
    ) -> Self {
        Self::build(store, provider, compiler, web_cache, None, None)
    }

    /// 附带本地 rules 目录的构造;编译请求上下文时会合并该目录下的 md 规则。
    pub fn with_local_rules(
        store: Store,
        provider: Arc<dyn Provider>,
        compiler: PromptCompiler,
        local_rules_dir: std::path::PathBuf,
    ) -> Self {
        Self::build(
            store,
            provider,
            compiler,
            WebCache::default(),
            None,
            Some(local_rules_dir),
        )
    }

    pub fn with_plugins(
        store: Store,
        provider: Arc<dyn Provider>,
        compiler: PromptCompiler,
        web_cache: WebCache,
        plugins: PluginRegistry,
        local_rules_dir: std::path::PathBuf,
    ) -> Self {
        Self::build(
            store,
            provider,
            compiler,
            web_cache,
            Some(plugins),
            Some(local_rules_dir),
        )
    }

    fn build(
        store: Store,
        provider: Arc<dyn Provider>,
        compiler: PromptCompiler,
        web_cache: WebCache,
        plugins: Option<PluginRegistry>,
        local_rules_dir: Option<std::path::PathBuf>,
    ) -> Self {
        Self {
            inner: Arc::new(RegistryInner {
                local: Mutex::new(HashMap::new()),
                next_local_generation: AtomicU64::new(1),
                upstream: Mutex::new(HashMap::new()),
                upstream_tasks: Mutex::new(HashMap::new()),
                next_upstream_generation: AtomicU64::new(1),
                route_changed: Notify::new(),
                traces: CursorTraceService::new(store.clone()),
                conversations: ConversationRegistry::new(
                    store.clone(),
                    provider,
                    compiler,
                    web_cache.clone(),
                    plugins.clone(),
                    local_rules_dir,
                ),
                store,
                web_cache,
                plugins,
            }),
        }
    }

    pub fn store(&self) -> &Store {
        &self.inner.store
    }

    pub fn trace(
        &self,
        request_id: &str,
    ) -> crate::cursor::services::observability::CursorTraceRecorder {
        self.inner.traces.recorder(request_id)
    }

    pub fn web_cache(&self) -> &WebCache {
        &self.inner.web_cache
    }

    pub fn plugins(&self) -> Option<&PluginRegistry> {
        self.inner.plugins.as_ref()
    }

    pub fn conversations(&self) -> &ConversationRegistry {
        &self.inner.conversations
    }

    pub async fn get_or_create(&self, request_id: &str) -> Result<TransportHandle> {
        self.get_or_create_for_append(request_id, false).await
    }

    pub(crate) async fn get_or_create_for_append(
        &self,
        request_id: &str,
        replace_closing: bool,
    ) -> Result<TransportHandle> {
        let mut local = self.inner.local.lock().await;
        if let Some(transport) = local.get(request_id) {
            if transport.handle.accepting_appends() || !replace_closing {
                return Ok(transport.handle.clone());
            }
        }
        local.remove(request_id);
        let (commands, receiver) = mpsc::channel(128);
        let output = Arc::new(OutputHub::default());
        let trace = self.inner.traces.recorder(request_id);
        trace.resume();
        let handle = TransportHandle::new(request_id.into(), commands, output, trace);
        let generation = self
            .inner
            .next_local_generation
            .fetch_add(1, Ordering::Relaxed);
        local.insert(
            request_id.into(),
            LocalTransport {
                generation,
                handle: handle.clone(),
            },
        );
        drop(local);
        self.inner.route_changed.notify_waiters();
        self.inner
            .conversations
            .bind_transport(handle.clone(), receiver);

        let registry = Arc::downgrade(&self.inner);
        let request_id = request_id.to_string();
        let lifecycle = handle.clone();
        tokio::spawn(async move {
            lifecycle.wait_transport_closed().await;
            if let Some(registry) = registry.upgrade() {
                let mut local = registry.local.lock().await;
                if local
                    .get(&request_id)
                    .is_some_and(|transport| transport.generation == generation)
                {
                    local.remove(&request_id);
                }
            }
        });
        Ok(handle)
    }

    pub async fn local(&self, request_id: &str) -> Option<TransportHandle> {
        self.inner
            .local
            .lock()
            .await
            .get(request_id)
            .map(|transport| transport.handle.clone())
    }

    pub async fn associate_upstream_task(
        &self,
        request_id: &str,
        parent: super::TransportParent,
        model: String,
    ) {
        let Some(handle) = self.local(&parent.request_id).await else {
            return;
        };
        let (reply, response) = tokio::sync::oneshot::channel();
        if handle
            .command(
                crate::cursor::conversation::TransportCommand::OfficialTaskOwner {
                    tool_call_id: parent.tool_call_id,
                    reply,
                },
            )
            .await
            .is_err()
        {
            return;
        }
        let Ok(Some((owner, exec_id))) = response.await else {
            return;
        };
        let generation = self.inner.upstream.lock().await.get(request_id).copied();
        if let Some(generation) = generation {
            self.inner.upstream_tasks.lock().await.insert(
                request_id.into(),
                OfficialTask {
                    generation,
                    parent_request_id: parent.request_id,
                    owner,
                    exec_id,
                    model,
                },
            );
        }
    }

    pub async fn fail_upstream_task(
        &self,
        request_id: &str,
        generation: u64,
        error: &crate::cursor::services::official_error::UpstreamError,
    ) {
        let mut tasks = self.inner.upstream_tasks.lock().await;
        let Some(task) = tasks.remove(request_id) else {
            return;
        };
        if task.generation != generation {
            // 代际不匹配:属于更新一轮的任务,放回不失败。
            tasks.insert(request_id.into(), task);
            return;
        }
        drop(tasks);
        let message = if crate::cursor::services::official_error::is_model_not_found(error) {
            crate::cursor::services::official_error::model_not_found_task_error(&task.model)
        } else {
            let mut message = format!(
                "{}: {}",
                error.code.as_deref().unwrap_or("upstream_error"),
                error.message
            );
            for detail in error
                .details
                .iter()
                .filter_map(|entry| entry.decoded.as_ref())
            {
                for text in [detail.title.as_deref(), detail.detail.as_deref()]
                    .into_iter()
                    .flatten()
                    .filter(|text| !text.is_empty())
                {
                    message.push('\n');
                    message.push_str(text);
                }
            }
            message
        };
        if let Some(handle) = self.local(&task.parent_request_id).await {
            let _ = handle
                .command(
                    crate::cursor::conversation::TransportCommand::OfficialTaskFailed {
                        owner: task.owner,
                        exec_id: task.exec_id,
                        message,
                    },
                )
                .await;
        }
    }

    pub async fn mark_upstream(&self, request_id: &str) {
        let mut upstream = self.inner.upstream.lock().await;
        let generation = self
            .inner
            .next_upstream_generation
            .fetch_add(1, Ordering::Relaxed);
        upstream.insert(request_id.into(), generation);
        drop(upstream);
        self.inner.route_changed.notify_waiters();
    }

    pub async fn upstream(&self, request_id: &str) -> bool {
        self.inner.upstream.lock().await.contains_key(request_id)
    }

    pub async fn wait_route(&self, request_id: &str) -> TransportRoute {
        loop {
            let changed = self.inner.route_changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if self.inner.local.lock().await.contains_key(request_id) {
                return TransportRoute::Local;
            }
            if let Some(generation) = self.inner.upstream.lock().await.get(request_id).copied() {
                return TransportRoute::Upstream(generation);
            }
            changed.await;
        }
    }

    pub fn finish_upstream(&self, request_id: String, generation: u64) {
        let registry = self.clone();
        tokio::spawn(async move {
            let mut upstream = registry.inner.upstream.lock().await;
            if upstream.get(&request_id) == Some(&generation) {
                upstream.remove(&request_id);
                registry
                    .inner
                    .upstream_tasks
                    .lock()
                    .await
                    .remove(&request_id);
            }
        });
    }

    pub async fn shutdown(&self) {
        self.inner.conversations.shutdown().await;
        let handles = std::mem::take(&mut *self.inner.local.lock().await);
        self.inner.upstream.lock().await.clear();
        self.inner.upstream_tasks.lock().await.clear();
        for transport in handles.into_values() {
            transport.handle.disconnect().await;
            let _ = tokio::time::timeout(
                std::time::Duration::from_secs(2),
                transport.handle.wait_transport_closed(),
            )
            .await;
        }
    }
}
