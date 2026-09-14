//! Verifies message delivery before, during, and after a Run.
mod support;

use std::collections::HashMap;

use cursor_server::{
    cursor::{protocol::proto::agent::v1 as pb, TransportCommand, TransportHandle},
    model::{ContentPart, MessageContent, ProjectedContent, Role},
};
use prost::Message;
use support::{
    drive, registry, request_context_success, run_request, stream_close, temp_store, text_response,
    wait_for_provider_requests, FakeProvider,
};

const FOLLOW_UP: &str = "Perform any necessary follow-up actions in response to the subagent completion above. If no follow-up work is needed, no further action is required. If you mention an agent or subagent in your response, link it with the `[Name](id)` Don't use generic label such as `[agent]`, `[worker]`, or `[subagent]`.";
const SHELL_FOLLOW_UP: &str = "Briefly inform the user about the task result and perform any follow-up actions (if needed). If there's no follow-ups needed, don't explicitly say that.";

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn runtime_completion_actions_join_concurrent_parent_tool_rounds() {
    let jobs = (0..8).map(|index| async move {
        let (_directory, store) = temp_store().await;
        let provider = FakeProvider::default();
        provider.push(support::tool_response(
            "read-call",
            "read",
            "Read",
            r#"{"path":"fixture"}"#,
        ));
        provider.push(text_response("follow-up", "processed completion"));
        provider.push(text_response(
            "late-follow-up",
            "processed completion after finalization",
        ));
        let registry = registry(store.clone(), provider.clone());
        let handle = registry
            .get_or_create(&format!("runtime-{index}"))
            .await
            .unwrap();
        let mut output = handle.subscribe();
        handle
            .command(TransportCommand::Append {
                seqno: 0,
                message: Box::new(run_request(
                    "parent-conversation",
                    "parent",
                    "test-model",
                    Some(pb::ConversationStateStructure {
                        subagent_runs_by_parent_tool_call_id: HashMap::from([(
                            "child-call".into(),
                            pb::SubagentRunState {
                                parent_tool_call_id: "child-call".into(),
                                subagent_id: Some(format!("child-{index}")),
                                task_id: Some(format!("child-{index}")),
                                status: pb::SubagentRunStatus::Backgrounded as i32,
                                ..Default::default()
                            },
                        )]),
                        ..Default::default()
                    }),
                    support::user_message_action(
                        "inspect",
                        "user",
                        Some(pb::RequestContext::default()),
                    ),
                )),
            })
            .await
            .unwrap();
        let mut seqno = 1;
        let child = format!("child-{index}");
        let out = drive(&handle, &mut output, &mut seqno, |exec| {
            assert!(matches!(
                exec.message,
                Some(pb::exec_server_message::Message::ReadArgs(_))
            ));
            let notification = pb::AgentClientMessage {
                message: Some(pb::agent_client_message::Message::ConversationAction(
                    pb::ConversationAction {
                        action: Some(
                            pb::conversation_action::Action::BackgroundTaskCompletionAction(
                                pb::BackgroundTaskCompletionAction {
                                    completions: vec![subagent_completion(
                                        &child,
                                        "child-call",
                                        "completed in parallel",
                                    )],
                                },
                            ),
                        ),
                        ..Default::default()
                    },
                )),
            };
            vec![
                notification.clone(),
                notification,
                support::read_success(exec.id, "fixture", "read result"),
                stream_close(exec.id),
            ]
        })
        .await;
        assert_eq!(
            out.terminal,
            serde_json::json!({}),
            "runtime completion must not fail parent {index}"
        );
        let state = &out
            .checkpoints
            .last()
            .unwrap()
            .subagent_runs_by_parent_tool_call_id["child-call"];
        assert_eq!(
            state.status,
            pb::SubagentRunStatus::Success as i32,
            "active delivery must acknowledge terminal child state to the IDE"
        );
        assert_eq!(
            state.completion_reason,
            Some(pb::BackgroundTaskCompletionReason::TaskFinished as i32)
        );
        let requests = provider.requests();
        assert!((2..=3).contains(&requests.len()));
        let history = serde_json::to_string(&requests.last().unwrap().history).unwrap();
        assert!(history.contains(&child));
        let statuses: Vec<String> = sqlx::query_scalar("SELECT status FROM runs")
            .fetch_all(store.pool())
            .await
            .unwrap();
        assert!(
            statuses.iter().all(|status| status == "completed"),
            "notification must not cancel or fail the parent: {statuses:?}"
        );
        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM background_completion_claims")
            .fetch_one(store.pool())
            .await
            .unwrap();
        assert_eq!(count, 1, "duplicate notification must be committed once");
    });
    futures_util::future::join_all(jobs).await;
}

#[tokio::test]
async fn failed_completion_preparation_can_be_retried_without_losing_the_notification() {
    let (_directory, store) = temp_store().await;
    let provider = FakeProvider::default();
    provider.push(text_response("retry", "recovered completion"));
    let registry = registry(store.clone(), provider.clone());
    let first = registry.get_or_create("failed-prepare").await.unwrap();
    let mut request = completion_run(
        "retry-child",
        "first-run",
        pb::ConversationStateStructure::default(),
    );
    let Some(pb::agent_client_message::Message::RunRequest(run)) = request.message.as_mut() else {
        unreachable!()
    };
    run.requested_model = None;
    let mut output = first.subscribe();
    first
        .command(TransportCommand::Append {
            seqno: 0,
            message: Box::new(request),
        })
        .await
        .unwrap();
    let mut seqno = 1;
    let out = drive(&first, &mut output, &mut seqno, |exec| {
        vec![request_context_success(exec.id), stream_close(exec.id)]
    })
    .await;
    assert!(out.terminal["error"]["message"]
        .as_str()
        .unwrap()
        .contains("does not select a model"));
    let claims: i64 = sqlx::query_scalar("SELECT count(*) FROM background_completion_claims")
        .fetch_one(store.pool())
        .await
        .unwrap();
    assert_eq!(claims, 0, "preparation must not consume the notification");
    let retry = registry.get_or_create("retry-prepare").await.unwrap();
    drive_completion(
        &retry,
        completion_run(
            "retry-child",
            "retry-run",
            pb::ConversationStateStructure::default(),
        ),
    )
    .await;
    assert_eq!(provider.request_count(), 1);
}

#[tokio::test]
async fn foreground_task_result_consumes_a_later_background_notification() {
    let (_directory, store) = temp_store().await;
    let provider = FakeProvider::default();
    provider.push(support::tool_response(
        "task-model",
        "task-call",
        "Task",
        r#"{"description":"Inspect","prompt":"inspect"}"#,
    ));
    provider.push(text_response("parent-final", "already handled the result"));
    provider.push(text_response("unexpected-wakeup", "must not run"));
    let registry = registry(store.clone(), provider.clone());
    let parent = registry.get_or_create("foreground-parent").await.unwrap();
    let mut output = parent.subscribe();
    parent
        .command(TransportCommand::Append {
            seqno: 0,
            message: Box::new(run_request(
                "parent-conversation",
                "parent-run",
                "test-model",
                None,
                support::user_message_action(
                    "inspect",
                    "user",
                    Some(pb::RequestContext::default()),
                ),
            )),
        })
        .await
        .unwrap();
    let mut seqno = 1;
    let out = drive(&parent, &mut output, &mut seqno, |exec| {
        vec![
            support::subagent_result_success(exec.id, "finished-child"),
            stream_close(exec.id),
        ]
    })
    .await;
    assert_eq!(out.terminal, serde_json::json!({}));
    // EndStream is the completion barrier; the notification is sent strictly after it.
    let before = provider.request_count();
    let messages_before = store
        .load_current_messages(&cursor_server::model::ConversationId::new(
            "parent-conversation",
        ))
        .await
        .unwrap();
    let replay = registry.get_or_create("historical-replay").await.unwrap();
    drive_forwarded_completion(
        &replay,
        completion_run(
            "finished-child",
            "replay-run",
            out.checkpoints.last().cloned().unwrap(),
        ),
    )
    .await;
    assert_eq!(
        provider.request_count(),
        before,
        "already delivered foreground result must not wake the parent"
    );
    assert_eq!(
        store
            .load_current_messages(&cursor_server::model::ConversationId::new(
                "parent-conversation"
            ))
            .await
            .unwrap(),
        messages_before
    );
    let (_recovered_directory, recovered_store) = temp_store().await;
    for (id, data) in out.blobs {
        assert_eq!(
            recovered_store
                .put_blob(&data, &[])
                .await
                .unwrap()
                .as_bytes()
                .as_slice(),
            id.as_slice()
        );
    }
    let fresh_provider = FakeProvider::default();
    let recovered = support::registry(recovered_store.clone(), fresh_provider.clone());
    let replay = recovered
        .get_or_create("foreground-checkpoint-replay")
        .await
        .unwrap();
    drive_forwarded_completion(
        &replay,
        completion_run(
            "finished-child",
            "restored-run",
            out.checkpoints.last().cloned().unwrap(),
        ),
    )
    .await;
    assert_eq!(fresh_provider.request_count(), 0);
    let runs: i64 = sqlx::query_scalar("SELECT count(*) FROM runs")
        .fetch_one(recovered_store.pool())
        .await
        .unwrap();
    assert_eq!(runs, 0);
}

#[tokio::test]
async fn same_child_new_execution_can_complete_without_replaying_the_old_execution() {
    let (_directory, store) = temp_store().await;
    let provider = FakeProvider::default();
    provider.push(text_response("first", "handled first execution"));
    provider.push(text_response("second", "handled second execution"));
    let registry = registry(store.clone(), provider.clone());
    let first = registry.get_or_create("lifecycle-first").await.unwrap();
    let (checkpoint, _) = drive_completion(
        &first,
        completion_batch_run(
            "run-first",
            pb::ConversationStateStructure::default(),
            vec![subagent_completion(
                "same-child",
                "first-tool-call",
                "first result",
            )],
        ),
    )
    .await;
    let second = registry.get_or_create("lifecycle-second").await.unwrap();
    let mut output = second.subscribe();
    second
        .command(TransportCommand::Append {
            seqno: 0,
            message: Box::new(completion_batch_run(
                "run-second",
                checkpoint,
                vec![subagent_completion(
                    "same-child",
                    "second-tool-call",
                    "second result",
                )],
            )),
        })
        .await
        .unwrap();
    let mut seqno = 1;
    let out = drive(&second, &mut output, &mut seqno, |exec| {
        vec![request_context_success(exec.id), stream_close(exec.id)]
    })
    .await;
    assert_eq!(
        out.terminal,
        serde_json::json!({}),
        "a new execution is not a conflicting duplicate"
    );
    assert_eq!(provider.request_count(), 2);
    let conversation = cursor_server::model::ConversationId::new("parent-conversation");
    let messages = store.load_current_messages(&conversation).await.unwrap();
    let replay = registry
        .get_or_create("first-execution-replayed-after-second")
        .await
        .unwrap();
    drive_forwarded_completion(
        &replay,
        completion_batch_run(
            "old-run-replay",
            out.checkpoints.last().cloned().unwrap(),
            vec![subagent_completion(
                "same-child",
                "first-tool-call",
                "first result",
            )],
        ),
    )
    .await;
    assert_eq!(provider.request_count(), 2);
    assert_eq!(
        store.load_current_messages(&conversation).await.unwrap(),
        messages
    );
    let runs: i64 = sqlx::query_scalar("SELECT count(*) FROM runs")
        .fetch_one(store.pool())
        .await
        .unwrap();
    assert_eq!(runs, 2);
}

#[tokio::test]
async fn failed_completion_provider_run_can_retry_the_committed_notification() {
    let (_directory, store) = temp_store().await;
    let provider = FakeProvider::default();
    provider.push_error(cursor_server::Error::ProviderStatus {
        status: axum::http::StatusCode::UNAUTHORIZED,
        message: "test rejection".into(),
    });
    provider.push(text_response("retry", "handled after retry"));
    let registry = registry(store.clone(), provider.clone());
    let first = registry.get_or_create("provider-failure").await.unwrap();
    let mut output = first.subscribe();
    first
        .command(TransportCommand::Append {
            seqno: 0,
            message: Box::new(completion_run(
                "retry-provider-child",
                "failed-run",
                pb::ConversationStateStructure::default(),
            )),
        })
        .await
        .unwrap();
    let mut seqno = 1;
    let out = drive(&first, &mut output, &mut seqno, |exec| {
        vec![request_context_success(exec.id), stream_close(exec.id)]
    })
    .await;
    assert!(out.terminal.get("error").is_some());
    let second = registry.get_or_create("provider-retry").await.unwrap();
    drive_forwarded_completion(
        &second,
        completion_run(
            "retry-provider-child",
            "retry-run",
            out.checkpoints.last().cloned().unwrap(),
        ),
    )
    .await;
    assert_eq!(
        provider.request_count(),
        2,
        "committed input is not proof that its processing succeeded"
    );
    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM messages WHERE runtime_event_id LIKE 'background-completed:%'",
    )
    .fetch_one(store.pool())
    .await
    .unwrap();
    assert_eq!(count, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn historical_notifications_after_endstream_are_deduplicated_across_rebuilt_sessions() {
    let (_directory, store) = temp_store().await;
    let provider = FakeProvider::default();
    provider.push(text_response("first", "handled completion"));
    let original_registry = registry(store.clone(), provider.clone());
    let first = original_registry.get_or_create("original").await.unwrap();
    let (checkpoint, _) = drive_completion(
        &first,
        completion_run(
            "old-child",
            "original-run",
            pb::ConversationStateStructure::default(),
        ),
    )
    .await;
    let messages = store
        .load_current_messages(&cursor_server::model::ConversationId::new(
            "parent-conversation",
        ))
        .await
        .unwrap();
    let rebuilt = registry(store.clone(), provider.clone());
    let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(8));
    let mut jobs = tokio::task::JoinSet::new();
    for index in (0..8).rev() {
        let registry = rebuilt.clone();
        let checkpoint = checkpoint.clone();
        let barrier = barrier.clone();
        jobs.spawn(async move {
            let handle = registry
                .get_or_create(&format!("replay-{index}"))
                .await
                .unwrap();
            barrier.wait().await;
            drive_forwarded_completion(
                &handle,
                completion_run("old-child", &format!("replay-run-{index}"), checkpoint),
            )
            .await;
        });
    }
    while let Some(result) = jobs.join_next().await {
        result.unwrap();
    }
    assert_eq!(provider.request_count(), 1);
    let runs: i64 = sqlx::query_scalar("SELECT count(*) FROM runs")
        .fetch_one(store.pool())
        .await
        .unwrap();
    assert_eq!(runs, 1, "historical replay must not even claim another run");
    assert_eq!(
        store
            .load_current_messages(&cursor_server::model::ConversationId::new(
                "parent-conversation"
            ))
            .await
            .unwrap(),
        messages
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn simultaneous_first_delivery_of_one_completion_starts_one_provider_run() {
    let (_directory, store) = temp_store().await;
    let provider = FakeProvider::default();
    provider.push(text_response("only", "handled once"));
    let registry = registry(store.clone(), provider.clone());
    let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(8));
    let mut jobs = tokio::task::JoinSet::new();
    for index in 0..8 {
        let registry = registry.clone();
        let barrier = barrier.clone();
        jobs.spawn(async move {
            let handle = registry
                .get_or_create(&format!("first-race-{index}"))
                .await
                .unwrap();
            barrier.wait().await;
            drive_forwarded_completion(
                &handle,
                completion_run(
                    "raced-child",
                    &format!("race-run-{index}"),
                    pb::ConversationStateStructure::default(),
                ),
            )
            .await;
        });
    }
    while let Some(result) = jobs.join_next().await {
        result.unwrap();
    }
    assert_eq!(provider.request_count(), 1);
    let runs: i64 = sqlx::query_scalar("SELECT count(*) FROM runs")
        .fetch_one(store.pool())
        .await
        .unwrap();
    assert_eq!(runs, 1);
    let notifications: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM messages WHERE runtime_event_id LIKE 'background-completed:%'",
    )
    .fetch_one(store.pool())
    .await
    .unwrap();
    assert_eq!(notifications, 1);
}

#[tokio::test]
async fn checkpoint_recovery_restores_completion_receipts_without_a_previous_database() {
    let (_directory, store) = temp_store().await;
    let provider = FakeProvider::default();
    provider.push(text_response("original", "handled once"));
    let original = registry(store, provider);
    let handle = original.get_or_create("original-checkpoint").await.unwrap();
    let (checkpoint, blobs) = drive_completion(
        &handle,
        completion_run(
            "restored-child",
            "original-run",
            pb::ConversationStateStructure::default(),
        ),
    )
    .await;
    let (_recovered_directory, recovered_store) = temp_store().await;
    for (id, data) in blobs {
        assert_eq!(
            recovered_store
                .put_blob(&data, &[])
                .await
                .unwrap()
                .as_bytes()
                .as_slice(),
            id.as_slice()
        );
    }
    let provider = FakeProvider::default();
    let recovered = registry(recovered_store.clone(), provider.clone());
    let handle = recovered
        .get_or_create("recovered-completion")
        .await
        .unwrap();
    drive_forwarded_completion(
        &handle,
        completion_run("restored-child", "recovered-run", checkpoint),
    )
    .await;
    assert_eq!(provider.request_count(), 0);
    let runs: i64 = sqlx::query_scalar("SELECT count(*) FROM runs")
        .fetch_one(recovered_store.pool())
        .await
        .unwrap();
    assert_eq!(runs, 0);
}

#[tokio::test]
async fn background_subagent_completion_starts_a_simulated_parent_turn() {
    let (_directory, store) = temp_store().await;
    let provider = FakeProvider::default();
    provider.push(text_response("model-call", "followed up"));
    let registry = registry(store.clone(), provider.clone());
    let handle = registry.get_or_create("completion-request").await.unwrap();
    let (checkpoint, blobs) = drive_completion(
        &handle,
        completion_run(
            "child-id",
            "reusable-parent-run",
            pb::ConversationStateStructure {
                mode: Some(pb::AgentMode::Multitask as i32),
                ..Default::default()
            },
        ),
    )
    .await;

    let requests = provider.requests();
    assert_eq!(requests.len(), 1);
    let [runtime] = requests[0].history.as_slice() else {
        panic!("completion Run must add exactly one runtime message")
    };
    assert_eq!(runtime.role, Role::User);
    let ProjectedContent::Parts(parts) = &runtime.content else {
        panic!("completion context must be text")
    };
    let [ContentPart::Text { text }] = parts.as_slice() else {
        panic!("completion context must have one text part")
    };
    assert!(text.contains("kind: subagent"));
    assert!(text.contains("agent_id: child-id"));
    assert!(text.contains("child result"));
    assert!(text.contains(FOLLOW_UP));

    let messages = store
        .load_current_messages(&cursor_server::model::ConversationId::new(
            "parent-conversation",
        ))
        .await
        .unwrap();
    assert!(messages.iter().any(|message| {
        message
            .runtime_event_id
            .as_deref()
            .is_some_and(|event_id| event_id.starts_with("background-completed:"))
            && matches!(&message.content, MessageContent::Parts { parts } if !parts.is_empty())
    }));

    let turn = pb::ConversationTurnStructure::decode(
        blobs
            .get(checkpoint.turns.last().expect("completion Turn"))
            .expect("completion Turn Blob")
            .as_slice(),
    )
    .unwrap();
    let pb::conversation_turn_structure::Turn::AgentConversationTurn(turn) = turn.turn.unwrap()
    else {
        panic!("expected agent conversation Turn")
    };
    let user = pb::UserMessage::decode(
        blobs
            .get(&turn.user_message)
            .expect("simulated UserMessage Blob")
            .as_slice(),
    )
    .unwrap();
    assert!(user.text.contains(FOLLOW_UP));
    assert_eq!(user.is_simulated_msg, Some(true));
    assert_eq!(
        user.simulated_msg_reason,
        Some(pb::SimulatedMsgReason::BackgroundTaskCompletion as i32)
    );
    assert_eq!(
        user.simulated_message_metadata.unwrap().task_id.as_deref(),
        Some("child-id")
    );

    assert_eq!(provider.requests().len(), 1);
}

#[tokio::test]
async fn background_completion_joins_the_active_run_instead_of_replacing_it() {
    let (_directory, store) = temp_store().await;
    let provider = FakeProvider::default();
    let first_ready = provider.push_gated(text_response("model-call-1", "first response"));
    provider.push(text_response("model-call-2", "processed both completions"));
    let registry = registry(store.clone(), provider.clone());
    let first = registry.get_or_create("active-completion-1").await.unwrap();
    let first_run = tokio::spawn(async move {
        drive_completion(
            &first,
            completion_run(
                "child-1",
                "parent-run-1",
                pb::ConversationStateStructure::default(),
            ),
        )
        .await
    });
    let second = registry.get_or_create("active-completion-2").await.unwrap();
    let mut wait_output = second.subscribe();
    let mut wait_seqno = 0;
    wait_for_provider_requests(&provider, &second, &mut wait_output, &mut wait_seqno, 1).await;

    let second_run = tokio::spawn(async move {
        drive_forwarded_completion(
            &second,
            completion_run(
                "child-2",
                "parent-run-2",
                pb::ConversationStateStructure::default(),
            ),
        )
        .await
    });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    first_ready.notify_one();
    second_run.await.unwrap();
    first_run.await.unwrap();
    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    let history = serde_json::to_string(&requests[1].history).unwrap();
    assert!(history.contains("child-1"));
    assert!(history.contains("first response"));
    assert!(history.contains("child-2"));
    let statuses: Vec<String> = sqlx::query_scalar(
        "SELECT status FROM runs WHERE conversation_id = 'parent-conversation' ORDER BY created_at_ms",
    )
    .fetch_all(store.pool())
    .await
    .unwrap();
    assert_eq!(statuses, ["completed"]);
}

#[tokio::test]
async fn retrying_one_background_completion_reuses_its_runtime_message() {
    let (_directory, store) = temp_store().await;
    let provider = FakeProvider::default();
    provider.push(text_response("model-call", "followed up"));
    let registry = registry(store.clone(), provider.clone());
    let first = registry.get_or_create("completion-retry-1").await.unwrap();
    let (_checkpoint, _) = drive_completion(
        &first,
        completion_run(
            "retry-child",
            "completion-retry-run-1",
            pb::ConversationStateStructure {
                mode: Some(pb::AgentMode::Multitask as i32),
                ..Default::default()
            },
        ),
    )
    .await;

    let second = registry.get_or_create("completion-retry-2").await.unwrap();
    drive_ignored_completion(
        &second,
        completion_run(
            "retry-child",
            "completion-retry-run-2",
            pb::ConversationStateStructure::default(),
        ),
    )
    .await;

    assert_eq!(provider.requests().len(), 1);
    let messages = store
        .load_current_messages(&cursor_server::model::ConversationId::new(
            "parent-conversation",
        ))
        .await
        .unwrap();
    assert_eq!(
        messages
            .iter()
            .filter(|message| {
                message
                    .runtime_event_id
                    .as_deref()
                    .is_some_and(|event_id| event_id.starts_with("background-completed:"))
            })
            .count(),
        1
    );
}

#[tokio::test]
async fn conflicting_terminal_status_fails_without_another_provider_call() {
    let (_directory, store) = temp_store().await;
    let provider = FakeProvider::default();
    provider.push(text_response("first-call", "processed success"));
    let registry = registry(store, provider.clone());
    let first = registry.get_or_create("conflict-request-1").await.unwrap();
    let (checkpoint, _) = drive_completion(
        &first,
        completion_run(
            "conflict-task",
            "conflict-run-1",
            pb::ConversationStateStructure::default(),
        ),
    )
    .await;

    let mut conflicting = subagent_completion("conflict-task", "task-call", "failed later");
    conflicting.status = pb::BackgroundTaskStatus::Error as i32;
    let second = registry.get_or_create("conflict-request-2").await.unwrap();
    drive_failed_completion(
        &second,
        completion_batch_run("conflict-run-2", checkpoint, vec![conflicting]),
    )
    .await;

    assert_eq!(provider.requests().len(), 1);
}

#[tokio::test]
async fn missing_conversation_id_keeps_completion_claims_request_scoped() {
    let (_directory, store) = temp_store().await;
    let provider = FakeProvider::default();
    provider.push(text_response("missing-1", "first"));
    provider.push(text_response("missing-2", "second"));
    let registry = registry(store, provider.clone());

    for request_id in ["missing-conversation-1", "missing-conversation-2"] {
        let handle = registry.get_or_create(request_id).await.unwrap();
        let mut request = completion_run(
            "same-task",
            request_id,
            pb::ConversationStateStructure::default(),
        );
        let Some(pb::agent_client_message::Message::RunRequest(run)) = request.message.as_mut()
        else {
            unreachable!()
        };
        run.conversation_id = None;
        drive_completion(&handle, request).await;
    }

    assert_eq!(provider.requests().len(), 2);
}

#[tokio::test]
async fn persisted_await_consumption_does_not_start_another_parent_turn() {
    let (_directory, store) = temp_store().await;
    let provider = FakeProvider::default();
    let registry = registry(store.clone(), provider.clone());
    store
        .ensure_conversation(&cursor_server::model::ConversationId::new(
            "parent-conversation",
        ))
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO background_completion_claims(
             conversation_id, task_kind, task_id, tool_call_id, terminal_status, disposition, created_at_ms, processed
         ) VALUES ('parent-conversation', 'subagent', 'consumed-child', 'task-call', 'success', 'consumed', 1, 1)",
    )
    .execute(store.pool())
    .await
    .unwrap();
    let handle = registry.get_or_create("consumed-completion").await.unwrap();
    let state = pb::ConversationStateStructure {
        subagent_runs_by_parent_tool_call_id: HashMap::from([(
            "task-call".into(),
            pb::SubagentRunState {
                parent_tool_call_id: "task-call".into(),
                subagent_id: Some("consumed-child".into()),
                status: pb::SubagentRunStatus::Success as i32,
                completion_reason: Some(pb::BackgroundTaskCompletionReason::TaskFinished as i32),
                ..Default::default()
            },
        )]),
        ..Default::default()
    };

    drive_ignored_completion(
        &handle,
        completion_run("consumed-child", "consumed-parent-run", state),
    )
    .await;
    let retry = registry
        .get_or_create("consumed-completion-retry")
        .await
        .unwrap();
    drive_ignored_completion(
        &retry,
        completion_run(
            "consumed-child",
            "consumed-parent-run-retry",
            pb::ConversationStateStructure::default(),
        ),
    )
    .await;

    assert!(provider.requests().is_empty());
    let run_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM runs WHERE conversation_id = 'parent-conversation'",
    )
    .fetch_one(store.pool())
    .await
    .unwrap();
    assert_eq!(run_count, 0);
}

#[tokio::test]
async fn partially_consumed_batch_projects_each_remaining_lifecycle_once() {
    let (_directory, store) = temp_store().await;
    let provider = FakeProvider::default();
    provider.push(text_response("batch-call", "processed remaining task"));
    let registry = registry(store.clone(), provider.clone());
    store
        .ensure_conversation(&cursor_server::model::ConversationId::new(
            "parent-conversation",
        ))
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO background_completion_claims(
             conversation_id, task_kind, task_id, tool_call_id, terminal_status, disposition, created_at_ms, processed
         ) VALUES ('parent-conversation', 'subagent', 'task-1', 'shared-call', 'success', 'consumed', 1, 1)",
    )
    .execute(store.pool())
    .await
    .unwrap();

    let action = vec![
        subagent_completion("task-1", "shared-call", "already awaited"),
        subagent_completion("task-2", "shared-call", "new result"),
    ];
    let first = registry.get_or_create("batch-request-1").await.unwrap();
    drive_forwarded_completion(
        &first,
        completion_batch_run(
            "batch-run-1",
            pb::ConversationStateStructure::default(),
            action.clone(),
        ),
    )
    .await;
    let retry = registry.get_or_create("batch-request-2").await.unwrap();
    let mut output = retry.subscribe();
    let mut seqno = 0;
    wait_for_provider_requests(&provider, &retry, &mut output, &mut seqno, 1).await;
    drive_ignored_completion(
        &retry,
        completion_batch_run(
            "batch-run-2",
            pb::ConversationStateStructure::default(),
            action,
        ),
    )
    .await;

    assert_eq!(provider.requests().len(), 1);
    let history = serde_json::to_string(&provider.requests()[0].history).unwrap();
    assert!(!history.contains("already awaited"));
    assert!(history.contains("new result"));
    let projected: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM background_completion_claims
         WHERE conversation_id = 'parent-conversation' AND disposition = 'projected'",
    )
    .fetch_one(store.pool())
    .await
    .unwrap();
    assert_eq!(projected, 1);
}

#[tokio::test]
async fn background_shell_completion_wakes_the_parent_with_the_captured_notification() {
    let (_directory, store) = temp_store().await;
    let provider = FakeProvider::default();
    provider.push(text_response(
        "shell-wakeup",
        "The background server was stopped.",
    ));
    let registry = registry(store, provider.clone());
    let handle = registry
        .get_or_create("shell-completion-request")
        .await
        .unwrap();
    let (checkpoint, blobs) = drive_completion(
        &handle,
        shell_completion_run(pb::ConversationStateStructure {
            mode: Some(pb::AgentMode::Agent as i32),
            ..Default::default()
        }),
    )
    .await;

    let requests = provider.requests();
    let [runtime] = requests[0].history.as_slice() else {
        panic!("Shell completion Run must add exactly one runtime message")
    };
    let ProjectedContent::Parts(parts) = &runtime.content else {
        panic!("Shell completion context must be text")
    };
    let [ContentPart::Text { text }] = parts.as_slice() else {
        panic!("Shell completion context must have one text part")
    };
    assert!(text.contains("<system_notification>"));
    assert!(text.contains("kind: shell"));
    assert!(text.contains("status: aborted"));
    assert!(text.contains("task_id: 977679"));
    assert!(text.contains("detail: terminated_by_user"));
    assert!(text.contains("output_path: /tmp/977679.txt"));
    assert!(text.contains(SHELL_FOLLOW_UP));
    assert!(text.starts_with("<timestamp>"));
    assert!(!text.contains("You are still in **Agent Mode**"));
    assert!(text.find("<system_notification>").unwrap() < text.find("<user_query>").unwrap());

    let turn = pb::ConversationTurnStructure::decode(
        blobs
            .get(checkpoint.turns.last().expect("Shell completion Turn"))
            .expect("Shell completion Turn Blob")
            .as_slice(),
    )
    .unwrap();
    let pb::conversation_turn_structure::Turn::AgentConversationTurn(turn) = turn.turn.unwrap()
    else {
        panic!("expected agent conversation Turn")
    };
    let user = pb::UserMessage::decode(
        blobs
            .get(&turn.user_message)
            .expect("simulated Shell UserMessage Blob")
            .as_slice(),
    )
    .unwrap();
    assert_eq!(user.text, *text);
    assert_eq!(user.is_simulated_msg, Some(true));
    assert_eq!(
        user.simulated_msg_reason,
        Some(pb::SimulatedMsgReason::BackgroundTaskCompletion as i32)
    );
    let metadata = user.simulated_message_metadata.unwrap();
    assert_eq!(
        metadata.title.as_deref(),
        Some("Start Python HTTP server on 9000")
    );
    assert_eq!(metadata.task_id.as_deref(), Some("977679"));
}

async fn drive_failed_completion(handle: &TransportHandle, message: pb::AgentClientMessage) {
    let mut output = handle.subscribe();
    handle
        .command(TransportCommand::Append {
            seqno: 0,
            message: Box::new(message),
        })
        .await
        .unwrap();
    let mut seqno = 1;
    let out = drive(handle, &mut output, &mut seqno, |_| vec![]).await;
    assert!(out.terminal["error"]["message"]
        .as_str()
        .unwrap()
        .contains("conflicting terminal completion"));
}

async fn drive_ignored_completion(handle: &TransportHandle, message: pb::AgentClientMessage) {
    let mut output = handle.subscribe();
    handle
        .command(TransportCommand::Append {
            seqno: 0,
            message: Box::new(message),
        })
        .await
        .unwrap();
    let mut seqno = 1;
    let out = drive(handle, &mut output, &mut seqno, |_| vec![]).await;
    assert_eq!(out.terminal, serde_json::json!({}));
}

async fn drive_completion(
    handle: &TransportHandle,
    message: pb::AgentClientMessage,
) -> (pb::ConversationStateStructure, HashMap<Vec<u8>, Vec<u8>>) {
    let mut output = handle.subscribe();
    handle
        .command(TransportCommand::Append {
            seqno: 0,
            message: Box::new(message),
        })
        .await
        .unwrap();
    let mut seqno = 1;
    let out = drive(handle, &mut output, &mut seqno, |exec| {
        assert!(matches!(
            exec.message,
            Some(pb::exec_server_message::Message::RequestContextArgs(_))
        ));
        vec![stream_close(exec.id), request_context_success(exec.id)]
    })
    .await;
    (
        out.checkpoints
            .iter()
            .rev()
            .find(|state| state.pending_tool_calls.is_empty())
            .expect("settled completion checkpoint")
            .clone(),
        out.blobs,
    )
}

async fn drive_forwarded_completion(handle: &TransportHandle, message: pb::AgentClientMessage) {
    let mut output = handle.subscribe();
    handle
        .command(TransportCommand::Append {
            seqno: 0,
            message: Box::new(message),
        })
        .await
        .unwrap();
    let mut seqno = 1;
    let out = drive(handle, &mut output, &mut seqno, |exec| {
        vec![stream_close(exec.id), request_context_success(exec.id)]
    })
    .await;
    assert_eq!(out.terminal, serde_json::json!({}));
}

fn completion_run(
    child_id: &str,
    run_id: &str,
    conversation_state: pb::ConversationStateStructure,
) -> pb::AgentClientMessage {
    completion_run_with_detail(child_id, run_id, conversation_state, "child result")
}

fn completion_run_with_detail(
    child_id: &str,
    run_id: &str,
    conversation_state: pb::ConversationStateStructure,
    detail: &str,
) -> pb::AgentClientMessage {
    completion_batch_run(
        run_id,
        conversation_state,
        vec![subagent_completion(child_id, "task-call", detail)],
    )
}

fn subagent_completion(
    task_id: &str,
    tool_call_id: &str,
    detail: &str,
) -> pb::BackgroundTaskCompletion {
    pb::BackgroundTaskCompletion {
        task_id: task_id.into(),
        kind: pb::BackgroundTaskKind::Subagent as i32,
        status: pb::BackgroundTaskStatus::Success as i32,
        title: "Inspect protocol".into(),
        detail: Some(detail.into()),
        output_path: Some("/tmp/child.jsonl".into()),
        reason: pb::BackgroundTaskCompletionReason::TaskFinished as i32,
        subagent_id: Some(task_id.into()),
        tool_call_id: Some(tool_call_id.into()),
        ..Default::default()
    }
}

fn completion_batch_run(
    run_id: &str,
    conversation_state: pb::ConversationStateStructure,
    completions: Vec<pb::BackgroundTaskCompletion>,
) -> pb::AgentClientMessage {
    run_request(
        "parent-conversation",
        run_id,
        "test-model",
        Some(conversation_state),
        pb::conversation_action::Action::BackgroundTaskCompletionAction(
            pb::BackgroundTaskCompletionAction { completions },
        ),
    )
}

fn shell_completion_run(
    conversation_state: pb::ConversationStateStructure,
) -> pb::AgentClientMessage {
    run_request(
        "parent-conversation",
        "shell-parent-run",
        "test-model",
        Some(conversation_state),
        pb::conversation_action::Action::BackgroundTaskCompletionAction(
            pb::BackgroundTaskCompletionAction {
                completions: vec![pb::BackgroundTaskCompletion {
                    task_id: "977679".into(),
                    kind: pb::BackgroundTaskKind::Shell as i32,
                    status: pb::BackgroundTaskStatus::Aborted as i32,
                    title: "Start Python HTTP server on 9000".into(),
                    detail: Some("terminated_by_user".into()),
                    output_path: Some("/tmp/977679.txt".into()),
                    reason: pb::BackgroundTaskCompletionReason::TaskFinished as i32,
                    tool_call_id: Some("shell-call".into()),
                    ..Default::default()
                }],
            },
        ),
    )
}
