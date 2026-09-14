//! Maps conversation IDs to active conversation runtimes.

#[cfg(test)]
#[path = "registry_tests.rs"]
mod tests;

use std::{
    collections::HashMap,
    sync::{Arc, Weak},
};

use tokio::sync::{mpsc, Mutex, Notify, OwnedMutexGuard};

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
    completion_starts: HashMap<ConversationId, Weak<Mutex<()>>>,
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

    /// Serialize completion admission through activation, not provider execution.
    /// Without this boundary, two idle observations can create competing runs.
    pub(crate) async fn lock_completion_start(
        &self,
        conversation_id: &ConversationId,
    ) -> OwnedMutexGuard<()> {
        let lock = {
            let mut state = self.inner.state.lock().await;
            state
                .completion_starts
                .retain(|_, lock| lock.strong_count() > 0);
            match state
                .completion_starts
                .get(conversation_id)
                .and_then(Weak::upgrade)
            {
                Some(lock) => lock,
                None => {
                    let lock = Arc::new(Mutex::new(()));
                    state
                        .completion_starts
                        .insert(conversation_id.clone(), Arc::downgrade(&lock));
                    lock
                }
            }
        };
        lock.lock_owned().await
    }

    pub(crate) async fn activate(
        &self,
        conversation_id: ConversationId,
        run_id: RunId,
        handle: RunHandle,
    ) -> Vec<CompiledMessages> {
        let mut state = self.inner.state.lock().await;
        let pending = state
            .pending
            .remove(&conversation_id)
            .map(|mut pending| pending.drain().collect())
            .unwrap_or_default();
        let previous = state.current.insert(
            conversation_id,
            ActiveRun {
                run_id: run_id.clone(),
                handle,
            },
        );
        if let Some(previous) = previous.filter(|previous| previous.run_id != run_id) {
            previous.handle.cancel();
        }
        pending
    }

    pub async fn deliver(
        &self,
        conversation_id: &ConversationId,
        mut compiled: CompiledMessages,
    ) -> CommandResult {
        if compiled.delivery == MessageDelivery::Ignore {
            return CommandResult::Applied;
        }
        loop {
            let mut state = self.inner.state.lock().await;
            let Some(active) = state.current.get(conversation_id).cloned() else {
                state
                    .pending
                    .entry(conversation_id.clone())
                    .or_default()
                    .push(compiled);
                return CommandResult::RunEnded;
            };
            drop(state);
            if compiled
                .target_run_id
                .as_ref()
                .is_some_and(|target| target != &active.run_id)
            {
                return CommandResult::StaleTarget;
            }
            let pending = compiled.clone();
            let result = match compiled.delivery {
                MessageDelivery::Ignore => CommandResult::Applied,
                MessageDelivery::InsertMessages => {
                    active
                        .handle
                        .insert_messages(compiled.event_id, compiled.messages)
                        .await
                }
                MessageDelivery::BreakMessages => {
                    active
                        .handle
                        .break_messages(compiled.event_id, compiled.messages)
                        .await
                }
            };
            if matches!(result, CommandResult::RunClosing | CommandResult::RunEnded) {
                let mut state = self.inner.state.lock().await;
                if state
                    .current
                    .get(conversation_id)
                    .is_some_and(|current| current.run_id != active.run_id)
                {
                    compiled = pending;
                    continue;
                }
                state
                    .pending
                    .entry(conversation_id.clone())
                    .or_default()
                    .push(pending);
                #[cfg(test)]
                self.inner.changed.notify_waiters();
            }
            return result;
        }
    }

    pub(crate) async fn discard_pending_completions(
        &self,
        conversation_id: &ConversationId,
        completions: &[crate::model::TerminalCompletion],
    ) {
        let mut state = self.inner.state.lock().await;
        if let Some(pending) = state.pending.get_mut(conversation_id) {
            pending.remove_completions(completions);
            if pending.is_empty() {
                state.pending.remove(conversation_id);
            }
        }
    }

    pub(crate) async fn release(&self, conversation_id: &ConversationId, run_id: &RunId) {
        let mut state = self.inner.state.lock().await;
        let current = &mut state.current;
        if current
            .get(conversation_id)
            .is_some_and(|run| &run.run_id == run_id)
        {
            current.remove(conversation_id);
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
