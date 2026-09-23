//! Maps conversation IDs to active conversation runtimes.

use std::{collections::HashMap, sync::Arc};

use tokio::sync::{mpsc, Mutex, Notify};

use crate::{
    cursor::{prompting::PromptCompiler, transport::TransportHandle},
    model::{ConversationId, RunId},
    provider::Provider,
    run::{CommandResult, RunHandle},
    search::WebCache,
    store::Store,
};

use super::{CompiledMessages, MessageDelivery, PendingMessages, TransportCommand};

#[derive(Clone)]
pub struct ConversationRegistry {
    inner: Arc<RegistryInner>,
}

#[derive(Clone)]
pub(crate) struct ConversationDependencies {
    pub store: Store,
    pub provider: Arc<dyn Provider>,
    pub compiler: PromptCompiler,
    pub web_cache: WebCache,
    /// 插件注册表;Task 工具的可用模型清单与模型目录需要插件模型的生效轴。
    pub plugins: Option<crate::plugin::PluginRegistry>,
    /// 本地 rules 服务的 md 存储目录;编译请求上下文时合并其中的规则。
    pub local_rules_dir: Option<std::path::PathBuf>,
}

struct RegistryInner {
    state: Mutex<RegistryState>,
    changed: Notify,
    pub dependencies: ConversationDependencies,
}

#[derive(Default)]
struct RegistryState {
    current: HashMap<ConversationId, ActiveRun>,
    pending: HashMap<ConversationId, PendingMessages>,
}

#[derive(Clone)]
struct ActiveRun {
    run_id: RunId,
    handle: RunHandle,
}

impl ConversationRegistry {
    pub fn new(
        store: Store,
        provider: Arc<dyn Provider>,
        compiler: PromptCompiler,
        web_cache: WebCache,
        plugins: Option<crate::plugin::PluginRegistry>,
        local_rules_dir: Option<std::path::PathBuf>,
    ) -> Self {
        Self {
            inner: Arc::new(RegistryInner {
                state: Mutex::new(RegistryState::default()),
                changed: Notify::new(),
                dependencies: ConversationDependencies {
                    store,
                    provider,
                    compiler,
                    web_cache,
                    plugins,
                    local_rules_dir,
                },
            }),
        }
    }

    pub(crate) fn dependencies(&self) -> &ConversationDependencies {
        &self.inner.dependencies
    }

    pub(crate) fn bind_transport(
        &self,
        handle: TransportHandle,
        receiver: mpsc::Receiver<TransportCommand>,
    ) {
        super::ConversationRuntime::spawn(self.clone(), handle, receiver);
    }

    /// Publishes the replacement run and claims every queued injection under one lock.
    /// Deliveries therefore observe either the pending queue or the new active run,
    /// never the gap between draining and activation.
    pub(crate) async fn activate(
        &self,
        conversation_id: ConversationId,
        run_id: RunId,
        handle: RunHandle,
    ) -> Vec<CompiledMessages> {
        let mut state = self.inner.state.lock().await;
        let previous = state.current.insert(
            conversation_id.clone(),
            ActiveRun {
                run_id: run_id.clone(),
                handle,
            },
        );
        let pending = state
            .pending
            .remove(&conversation_id)
            .map(|mut pending| pending.drain().collect())
            .unwrap_or_default();
        drop(state);
        if let Some(previous) = previous.filter(|previous| previous.run_id != run_id) {
            previous.handle.cancel();
        }
        pending
    }

    pub async fn deliver(
        &self,
        conversation_id: &ConversationId,
        compiled: CompiledMessages,
    ) -> CommandResult {
        if compiled.delivery == MessageDelivery::Ignore {
            return CommandResult::Applied;
        }
        loop {
            let active = {
                let mut state = self.inner.state.lock().await;
                let Some(active) = state.current.get(conversation_id).cloned() else {
                    state
                        .pending
                        .entry(conversation_id.clone())
                        .or_default()
                        .push(compiled);
                    return CommandResult::RunEnded;
                };
                if compiled
                    .target_run_id
                    .as_ref()
                    .is_some_and(|target| target != &active.run_id)
                {
                    return CommandResult::StaleTarget;
                }
                active
            };
            let pending = compiled.clone();
            let result = match compiled.delivery {
                MessageDelivery::Ignore => CommandResult::Applied,
                MessageDelivery::InsertMessages => {
                    active
                        .handle
                        .insert_messages(compiled.event_id.clone(), compiled.messages.clone())
                        .await
                }
                MessageDelivery::BreakMessages => {
                    active
                        .handle
                        .break_messages(compiled.event_id.clone(), compiled.messages.clone())
                        .await
                }
            };
            if !matches!(result, CommandResult::RunClosing | CommandResult::RunEnded) {
                return result;
            }

            let mut state = self.inner.state.lock().await;
            let owner_unchanged = state
                .current
                .get(conversation_id)
                .is_some_and(|current| current.run_id == active.run_id);
            if owner_unchanged || !state.current.contains_key(conversation_id) {
                state
                    .pending
                    .entry(conversation_id.clone())
                    .or_default()
                    .push(pending);
                return result;
            }
            drop(state);
            // An untargeted delivery that lost ownership to a replacement run is
            // retried against that run. A targeted delivery is rejected on the
            // next loop iteration rather than being stranded in pending state.
        }
    }

    pub(crate) async fn release(&self, conversation_id: &ConversationId, run_id: &RunId) {
        let mut state = self.inner.state.lock().await;
        if state
            .current
            .get(conversation_id)
            .is_some_and(|run| &run.run_id == run_id)
        {
            state.current.remove(conversation_id);
            self.inner.changed.notify_waiters();
        }
    }

    pub(crate) async fn wait_until_idle(&self, conversation_id: &ConversationId) {
        loop {
            let changed = self.inner.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if !self
                .inner
                .state
                .lock()
                .await
                .current
                .contains_key(conversation_id)
            {
                return;
            }
            changed.await;
        }
    }

    pub async fn shutdown(&self) {
        let current = std::mem::take(&mut self.inner.state.lock().await.current);
        for active in current.into_values() {
            active.handle.cancel();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        cursor::prompting::PromptAssets, model::ModelInvocation, provider::ProviderStream,
    };
    use tokio_util::sync::CancellationToken;

    struct EmptyProvider;

    impl Provider for EmptyProvider {
        fn stream(
            &self,
            _invocation: ModelInvocation,
            _cancellation: CancellationToken,
        ) -> ProviderStream {
            Box::pin(futures_util::stream::empty())
        }
    }

    async fn registry() -> ConversationRegistry {
        ConversationRegistry::new(
            Store::connect("sqlite::memory:").await.unwrap(),
            Arc::new(EmptyProvider),
            PromptCompiler::new(PromptAssets::embedded().unwrap()),
            WebCache::default(),
            None,
            None,
        )
    }

    fn injection(event_id: &str) -> CompiledMessages {
        CompiledMessages {
            event_id: event_id.into(),
            target_run_id: None,
            messages: Vec::new(),
            delivery: MessageDelivery::InsertMessages,
        }
    }

    #[tokio::test]
    async fn concurrent_delivery_is_claimed_by_current_or_next_activation() {
        let registry = registry().await;
        let conversation_id = ConversationId::from("conversation");
        assert_eq!(
            registry
                .deliver(&conversation_id, injection("before"))
                .await,
            CommandResult::RunEnded
        );

        let (port, _session, first_handle) = crate::run::channel(RunId::from("run-1"), 1);
        drop(port);
        let barrier = Arc::new(tokio::sync::Barrier::new(3));
        let activating = {
            let registry = registry.clone();
            let conversation_id = conversation_id.clone();
            let barrier = barrier.clone();
            tokio::spawn(async move {
                barrier.wait().await;
                registry
                    .activate(conversation_id, RunId::from("run-1"), first_handle)
                    .await
            })
        };
        let delivering = {
            let registry = registry.clone();
            let conversation_id = conversation_id.clone();
            let barrier = barrier.clone();
            tokio::spawn(async move {
                barrier.wait().await;
                registry
                    .deliver(&conversation_id, injection("racing"))
                    .await
            })
        };
        barrier.wait().await;
        let mut claimed = activating.await.unwrap();
        assert_eq!(delivering.await.unwrap(), CommandResult::RunEnded);

        let (port, _session, second_handle) = crate::run::channel(RunId::from("run-2"), 1);
        drop(port);
        claimed.extend(
            registry
                .activate(conversation_id, RunId::from("run-2"), second_handle)
                .await,
        );
        let mut ids = claimed
            .into_iter()
            .map(|message| message.event_id)
            .collect::<Vec<_>>();
        ids.sort();
        assert_eq!(ids, ["before", "racing"]);
    }
}
