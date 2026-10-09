//! Connectivity testing for the control API: registration, cancellation and
//! completion cleanup of per-test runs have a single owner in this module.
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex, MutexGuard},
    time::Instant,
};

use futures_util::StreamExt;
use serde::Serialize;
use tokio_util::sync::CancellationToken;

use crate::{
    model::{
        ContentPart, ModelInvocation, ModelRequest, ModelSpec, ProjectedContent, ProjectedMessage,
        PromptSpec, Role,
    },
    provider::{is_valid_response_event, ModelEvent},
    Error, Result,
};

use super::ControlService;

#[derive(Clone, Debug, Serialize)]
pub struct ModelConnectivityResult {
    pub duration_ms: u64,
    pub first_valid_response_ms: Option<u64>,
    pub output_tokens: u64,
    pub tokens_per_second: f64,
    pub tokens_estimated: bool,
    pub output: String,
}

impl ControlService {
    /// 运行连通性测试并持有其取消注册:注册、运行、结束清理
    /// 全部由本方法独占完成,是注册表唯一的写入入口。
    pub async fn test_model(
        &self,
        model_hash: &str,
        test_id: &str,
    ) -> Result<ModelConnectivityResult> {
        let cancellation = self.register_model_test(test_id);
        let result = self.run_model_test(model_hash, cancellation).await;
        self.remove_model_test(test_id);
        result
    }

    pub fn cancel_model_test(&self, test_id: &str) {
        let cancellation = {
            let mut tests = self.lock_model_tests();
            tests.entry(test_id.to_owned()).or_default().clone()
        };
        cancellation.cancel();
    }

    /// 注册表的单一写入入口:同一 test_id 已有进行中的测试则复用其取消令牌,
    /// 否则登记新令牌;结束后由 [`Self::remove_model_test`] 清理。
    fn register_model_test(&self, test_id: &str) -> CancellationToken {
        self.lock_model_tests()
            .entry(test_id.to_owned())
            .or_default()
            .clone()
    }

    fn remove_model_test(&self, test_id: &str) {
        self.lock_model_tests().remove(test_id);
    }

    fn lock_model_tests(&self) -> MutexGuard<'_, BTreeMap<String, CancellationToken>> {
        self.model_tests
            .lock()
            .expect("model test registry mutex poisoned")
    }

    async fn run_model_test(
        &self,
        model_hash: &str,
        cancellation: CancellationToken,
    ) -> Result<ModelConnectivityResult> {
        const TEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(45);
        const TEST_PROMPT: &str = "Output the numbers 1 through 120 separated by a single space. No commas, no newlines, no explanation.";

        // 控制 API 是纯本地配置入口:按统一目录解析,未命中即校验错误。
        let directory =
            crate::model::ModelDirectory::configured(&self.store, Some(&self.plugins)).await?;
        let mut model = ModelSpec::new(model_hash);
        match directory.resolve(model_hash) {
            crate::model::Resolution::Matched { id, .. } => {
                let entry = directory
                    .get(&id)
                    .expect("matched id exists in the directory");
                model.model_id = id.clone();
                match entry.source {
                    crate::model::ModelSource::Builtin => {
                        let configured = self
                            .store
                            .model(&id)
                            .await?
                            .ok_or_else(|| Error::RunNotFound(format!("model {id}")))?;
                        configured.configure(&mut model);
                        model.max_output_tokens =
                            Some(configured.max_output_tokens().unwrap_or(65_536));
                    }
                    crate::model::ModelSource::Plugin => {
                        let descriptor = entry
                            .plugin
                            .as_ref()
                            .expect("plugin entries carry their descriptor");
                        model.display_name = Some(descriptor.display_name.clone());
                        model.max_output_tokens =
                            Some(descriptor.max_output_tokens.unwrap_or(65_536));
                    }
                }
            }
            crate::model::Resolution::Ambiguous { candidates } => {
                return Err(Error::Config(format!(
                    "model '{model_hash}' is ambiguous; use one of the model IDs: {}",
                    candidates.join(", ")
                )));
            }
            crate::model::Resolution::NotFound => {
                return Err(Error::RunNotFound(format!("model {model_hash}")));
            }
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

pub(super) fn new_model_tests_registry() -> Arc<Mutex<BTreeMap<String, CancellationToken>>> {
    Arc::new(Mutex::new(BTreeMap::new()))
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use crate::store::Store;

    use super::super::test_support::{create_test_model, CancellationProvider, TestProvider};
    use super::super::{AccessToken, AccessTokenSource, ControlService};

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
        let plugin_runtime = crate::plugin::PluginRuntime::managed().unwrap();
        let plugins = crate::plugin::PluginRegistry::managed(
            store.clone(),
            plugin_runtime.clone(),
            "test".into(),
        )
        .unwrap();
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
            crate::model::ProjectedContent::Parts(parts)
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
        let plugin_runtime = crate::plugin::PluginRuntime::managed().unwrap();
        let plugins = crate::plugin::PluginRegistry::managed(
            store.clone(),
            plugin_runtime.clone(),
            "test".into(),
        )
        .unwrap();
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
}
