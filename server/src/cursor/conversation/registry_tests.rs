//! Drives a same-connection completion across the real final checkpoint barrier.
use std::sync::atomic::{AtomicUsize, Ordering};

use bytes::Bytes;
use prost::Message;
use tokio_util::sync::CancellationToken;

use crate::{
    cursor::{
        compile,
        prompting::PromptAssets,
        protocol::{connect, proto::agent::v1 as pb},
        TransportRegistry,
    },
    model::ModelInvocation,
    provider::{FinishReason, ModelEvent, ProviderStream},
    run::RunPhase,
};

use super::*;

#[derive(Default)]
struct CountingProvider(AtomicUsize);

impl Provider for CountingProvider {
    fn stream(&self, _: ModelInvocation, _: CancellationToken) -> ProviderStream {
        let call = self.0.fetch_add(1, Ordering::SeqCst);
        Box::pin(futures_util::stream::iter([
            Ok(ModelEvent::Start {
                model_call_id: format!("model-{call}"),
            }),
            Ok(ModelEvent::TextStart),
            Ok(ModelEvent::TextDelta("handled completion".into())),
            Ok(ModelEvent::TextEnd),
            Ok(ModelEvent::Done(FinishReason::Stop)),
        ]))
    }
}

struct Client {
    handle: TransportHandle,
    output: mpsc::UnboundedReceiver<Bytes>,
    seqno: i64,
    checkpoint: Option<pb::ConversationStateStructure>,
}

impl Client {
    fn new(handle: TransportHandle) -> Self {
        let output = handle.subscribe();
        Self {
            handle,
            output,
            seqno: 0,
            checkpoint: None,
        }
    }

    async fn append(&mut self, message: pb::agent_client_message::Message) {
        self.handle
            .command(TransportCommand::Append {
                seqno: self.seqno,
                message: Box::new(pb::AgentClientMessage {
                    message: Some(message),
                }),
            })
            .await
            .unwrap();
        self.seqno += 1;
    }

    async fn receive(&mut self) -> Option<pb::AgentServerMessage> {
        let frame = self
            .output
            .recv()
            .await
            .expect("EndStream must precede channel close");
        let (flags, payload) = connect::decode_frames(&frame).unwrap().pop().unwrap();
        if flags & connect::END_STREAM_FLAG != 0 {
            assert_eq!(
                serde_json::from_slice::<serde_json::Value>(&payload).unwrap(),
                serde_json::json!({})
            );
            None
        } else {
            Some(pb::AgentServerMessage::decode(payload).unwrap())
        }
    }

    async fn acknowledge(&mut self, server: pb::AgentServerMessage) {
        match server.message {
            Some(pb::agent_server_message::Message::KvServerMessage(kv)) => {
                assert!(matches!(
                    kv.message,
                    Some(pb::kv_server_message::Message::SetBlobArgs(_))
                ));
                self.append(pb::agent_client_message::Message::KvClientMessage(
                    pb::KvClientMessage {
                        id: kv.id,
                        message: Some(pb::kv_client_message::Message::SetBlobResult(
                            pb::SetBlobResult { error: None },
                        )),
                    },
                ))
                .await;
            }
            Some(pb::agent_server_message::Message::ExecServerMessage(exec)) => {
                assert!(
                    matches!(
                        exec.message,
                        Some(pb::exec_server_message::Message::RequestContextArgs(_))
                    ),
                    "processed early return must not prepare another tool request"
                );
                self.append(pb::agent_client_message::Message::ExecClientMessage(
                    pb::ExecClientMessage {
                        id: exec.id,
                        message: Some(pb::exec_client_message::Message::RequestContextResult(
                            pb::RequestContextResult {
                                result: Some(pb::request_context_result::Result::Success(
                                    pb::RequestContextSuccess {
                                        request_context: Some(pb::RequestContext::default()),
                                        ..Default::default()
                                    },
                                )),
                            },
                        )),
                        ..Default::default()
                    },
                ))
                .await;
                self.append(pb::agent_client_message::Message::ExecClientControlMessage(
                    pb::ExecClientControlMessage {
                        message: Some(pb::exec_client_control_message::Message::StreamClose(
                            pb::ExecClientStreamClose { id: exec.id },
                        )),
                    },
                ))
                .await;
            }
            Some(pb::agent_server_message::Message::ConversationCheckpointUpdate(state)) => {
                self.checkpoint = Some(state)
            }
            _ => {}
        }
    }

    async fn finish(&mut self) {
        while let Some(server) = self.receive().await {
            self.acknowledge(server).await;
        }
    }
}

fn action(tool_call_id: &str) -> pb::ConversationAction {
    pb::ConversationAction {
        action: Some(
            pb::conversation_action::Action::BackgroundTaskCompletionAction(
                pb::BackgroundTaskCompletionAction {
                    completions: vec![pb::BackgroundTaskCompletion {
                        task_id: "child".into(),
                        subagent_id: Some("child".into()),
                        tool_call_id: Some(tool_call_id.into()),
                        kind: pb::BackgroundTaskKind::Subagent as i32,
                        reason: pb::BackgroundTaskCompletionReason::TaskFinished as i32,
                        status: pb::BackgroundTaskStatus::Success as i32,
                        title: "Inspect".into(),
                        detail: Some("result".into()),
                        ..Default::default()
                    }],
                },
            ),
        ),
        ..Default::default()
    }
}

fn request(
    tool_call_id: &str,
    checkpoint: Option<pb::ConversationStateStructure>,
) -> pb::AgentRunRequest {
    pb::AgentRunRequest {
        conversation_id: Some("parent".into()),
        run_id: Some("reusable-parent".into()),
        requested_model: Some(pb::RequestedModel {
            model_id: "test-model".into(),
            ..Default::default()
        }),
        action: Some(action(tool_call_id)),
        conversation_state: checkpoint,
        ..Default::default()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn finalizing_same_connection_replay_clears_pending_before_early_return() {
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::connect(&format!(
            "sqlite://{}",
            directory.path().join("test.db").display()
        ))
        .await
        .unwrap();
        let provider = Arc::new(CountingProvider::default());
        let assets = PromptAssets::load(
            &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("prompt/cursor"),
        )
        .unwrap();
        let transports =
            TransportRegistry::new(store.clone(), provider.clone(), PromptCompiler::new(assets));
        let registry = transports.conversations();
        let conversation = ConversationId::new("parent");
        let initial = request("old-execution", None);
        let completions = compile::background_terminal_completions(&initial)
            .unwrap()
            .unwrap();
        let mut client = Client::new(transports.get_or_create("same-connection").await.unwrap());
        client
            .append(pb::agent_client_message::Message::RunRequest(initial))
            .await;

        // Withhold the final assistant root's KV acknowledgement. The real
        // engine has entered Finalizing, but finish_run cannot yet set processed.
        let final_blob = loop {
            let server = client.receive().await.expect("final root before EndStream");
            let is_final_root = match &server.message {
                Some(pb::agent_server_message::Message::KvServerMessage(kv)) => match &kv.message {
                    Some(pb::kv_server_message::Message::SetBlobArgs(set)) => {
                        serde_json::from_slice::<serde_json::Value>(&set.blob_data)
                            .ok()
                            .is_some_and(|wire| wire["role"] == "assistant")
                    }
                    _ => false,
                },
                _ => false,
            };
            if is_final_root {
                break server;
            }
            client.acknowledge(server).await;
        };
        assert_eq!(
            registry.inner.state.lock().await.current[&conversation]
                .handle
                .phase(),
            RunPhase::Finalizing
        );
        assert!(!store
            .background_completions_processed(&conversation, &completions)
            .await
            .unwrap());
        let history = store.load_current_messages(&conversation).await.unwrap();
        assert_eq!(provider.0.load(Ordering::SeqCst), 1);

        // The same-connection runtime path must reach deliver -> RunClosing ->
        // pending before allowing the original final checkpoint to commit.
        client
            .append(pb::agent_client_message::Message::ConversationAction(
                action("old-execution"),
            ))
            .await;
        loop {
            let changed = registry.inner.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            let queued = registry
                .inner
                .state
                .lock()
                .await
                .pending
                .get(&conversation)
                .is_some_and(|pending| {
                    pending
                        .queued()
                        .iter()
                        .any(|batch| batch.event_id == completions[0].event_id)
                });
            if queued {
                break;
            }
            changed.await;
        }
        assert!(!store
            .background_completions_processed(&conversation, &completions)
            .await
            .unwrap());
        client.acknowledge(final_blob).await;
        client.finish().await;

        // Successful finish releases previous_finished, sends ContinueRequest,
        // and reaches spawn_run_request's earliest processed check.
        assert!(store
            .background_completions_processed(&conversation, &completions)
            .await
            .unwrap());
        let pending_count = registry
            .inner
            .state
            .lock()
            .await
            .pending
            .get(&conversation)
            .map_or(0, |pending| pending.queued().len());
        assert_eq!(
            pending_count, 0,
            "the processed continuation must remove its queued replay"
        );
        assert_eq!(provider.0.load(Ordering::SeqCst), 1);
        let runs: i64 = sqlx::query_scalar("SELECT count(*) FROM runs")
            .fetch_one(store.pool())
            .await
            .unwrap();
        assert_eq!(runs, 1);
        assert_eq!(
            store.load_current_messages(&conversation).await.unwrap(),
            history
        );

        // A genuinely new execution of the same child still reaches the provider.
        let mut next = Client::new(transports.get_or_create("new-execution").await.unwrap());
        next.append(pb::agent_client_message::Message::RunRequest(request(
            "new-execution",
            client.checkpoint,
        )))
        .await;
        next.finish().await;
        assert_eq!(provider.0.load(Ordering::SeqCst), 2);
        let runs: i64 = sqlx::query_scalar("SELECT count(*) FROM runs")
            .fetch_one(store.pool())
            .await
            .unwrap();
        assert_eq!(runs, 2);
        let current = store.load_current_messages(&conversation).await.unwrap();
        assert_eq!(&current[..history.len()], history.as_slice());
        assert_eq!(
            current
                .iter()
                .filter(|message| message.terminal_completion.is_some())
                .count(),
            2
        );
    })
    .await
    .expect("channel-driven finalization scenario completed");
}
