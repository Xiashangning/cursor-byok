//! Verifies message delivery before, during, and after a Run.
mod support;

use std::collections::HashMap;

use cursor_server::{
    cursor::{
        protocol::connect, protocol::proto::agent::v1 as pb, TransportCommand, TransportHandle,
    },
    model::{ContentPart, MessageContent, ProjectedContent, Role},
};
use prost::Message;
use support::{
    drive, kv_ack, registry, request_context_success, run_request, stream_close, temp_store,
    text_response, wait_for_provider_requests, FakeProvider,
};

const FOLLOW_UP: &str = "Perform any necessary follow-up actions in response to the subagent completion above. If no follow-up work is needed, no further action is required. If you mention an agent or subagent in your response, link it with the `[Name](id)` Don't use generic label such as `[agent]`, `[worker]`, or `[subagent]`.";
const SHELL_FOLLOW_UP: &str = "Briefly inform the user about the task result and perform any follow-up actions (if needed). If there's no follow-ups needed, don't explicitly say that.";

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
        message.runtime_event_id.as_deref()
            == Some("background-completed:BACKGROUND_TASK_KIND_SUBAGENT:child-id:task-call")
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

    provider.push(text_response("model-call-2", "followed up again"));
    let second = registry
        .get_or_create("completion-request-2")
        .await
        .unwrap();
    drive_completion(
        &second,
        completion_run("child-id-2", "reusable-parent-run-2", checkpoint),
    )
    .await;

    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    let runtime_ids = requests[1]
        .history
        .iter()
        .map(|message| message.message_id.as_str())
        .filter(|id| id.starts_with("runtime:"))
        .collect::<Vec<_>>();
    assert_eq!(
        runtime_ids,
        [
            "runtime:background-completed:BACKGROUND_TASK_KIND_SUBAGENT:child-id:task-call",
            "runtime:background-completed:BACKGROUND_TASK_KIND_SUBAGENT:child-id-2:task-call"
        ]
    );
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
    let (checkpoint, _) = drive_completion(
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

    // 总结已提交后的 at-least-once 重投:不再激活模型、不建 Run、静默 Success。
    provider.push(text_response("model-call-2", "followed up again"));
    let second = registry.get_or_create("completion-retry-2").await.unwrap();
    let mut output = second.subscribe();
    second
        .command(TransportCommand::Append {
            seqno: 0,
            message: Box::new(completion_run(
                "retry-child",
                "completion-retry-run-2",
                checkpoint,
            )),
        })
        .await
        .unwrap();
    let mut seqno = 1;
    let out = drive(&second, &mut output, &mut seqno, |exec| {
        assert_eq!(exec.id, 0);
        vec![stream_close(0), request_context_success(0)]
    })
    .await;
    assert_eq!(out.terminal, serde_json::json!({}));
    assert_eq!(
        provider.requests().len(),
        1,
        "covered redelivery must not activate the model again"
    );
    let run_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM runs WHERE conversation_id = 'parent-conversation'",
    )
    .fetch_one(store.pool())
    .await
    .unwrap();
    assert_eq!(run_count, 1, "covered redelivery must not create a Run");

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
                message.runtime_event_id.as_deref()
                    == Some(
                        "background-completed:BACKGROUND_TASK_KIND_SUBAGENT:retry-child:task-call",
                    )
            })
            .count(),
        1
    );
    let tail = messages.last().expect("summary tail");
    assert!(
        matches!(&tail.content, MessageContent::Assistant { text, .. } if text.contains("followed up")),
        "the committed summary must remain the conversation tail"
    );
}

#[tokio::test]
async fn redelivering_after_a_crash_reruns_the_follow_up_exactly_once() {
    let (_directory, store) = temp_store().await;
    let provider = FakeProvider::default();
    provider.push_pending();
    let registry = registry(store.clone(), provider.clone());
    let first = registry.get_or_create("crash-redelivery-1").await.unwrap();
    let mut output = first.subscribe();
    first
        .command(TransportCommand::Append {
            seqno: 0,
            message: Box::new(completion_run(
                "crash-child",
                "crash-run-1",
                pb::ConversationStateStructure::default(),
            )),
        })
        .await
        .unwrap();
    let mut seqno = 1;
    let mut checkpoints = Vec::new();
    // 崩溃窗口:通知已提交(InitialMessages checkpoint 已发布)但总结未提交。
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    while provider.request_count() < 1 || checkpoints.is_empty() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "notification run did not reach the model"
        );
        if let Ok(Some(frame)) =
            tokio::time::timeout(std::time::Duration::from_millis(20), output.recv()).await
        {
            pump_frame(&first, &mut seqno, &frame, &mut checkpoints).await;
        }
    }
    first
        .command(TransportCommand::Append {
            seqno,
            message: Box::new(cancel_action()),
        })
        .await
        .unwrap();
    seqno += 1;
    loop {
        let frame = tokio::time::timeout(std::time::Duration::from_secs(5), output.recv())
            .await
            .unwrap()
            .expect("RunSSE closed before EndStream");
        let (flags, payload) = connect::decode_frames(&frame).unwrap().pop().unwrap();
        if flags & connect::END_STREAM_FLAG != 0 {
            let terminal: serde_json::Value = serde_json::from_slice(&payload).unwrap();
            assert_eq!(terminal["error"]["code"], "canceled");
            break;
        }
        pump_frame(&first, &mut seqno, &frame, &mut checkpoints).await;
    }
    let checkpoint = checkpoints
        .iter()
        .rev()
        .find(|state| state.pending_tool_calls.is_empty())
        .expect("notification checkpoint")
        .clone();

    // 通知在客户端历史里但没有总结:覆盖检查不得抑制重跑(崩溃自愈)。
    provider.push(text_response("model-call-2", "recovered summary"));
    let second = registry.get_or_create("crash-redelivery-2").await.unwrap();
    drive_completion(
        &second,
        completion_run("crash-child", "crash-run-2", checkpoint),
    )
    .await;

    let requests = provider.requests();
    assert_eq!(
        requests.len(),
        2,
        "crash redelivery must rerun the follow-up exactly once"
    );
    let history = serde_json::to_string(&requests[1].history).unwrap();
    assert!(history.contains("crash-child"));
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
                message.runtime_event_id.as_deref()
                    == Some(
                        "background-completed:BACKGROUND_TASK_KIND_SUBAGENT:crash-child:task-call",
                    )
            })
            .count(),
        1
    );
    let tail = messages.last().expect("summary tail");
    assert!(
        matches!(&tail.content, MessageContent::Assistant { text, .. } if text.contains("recovered summary")),
        "the rerun must commit the follow-up summary"
    );
    let statuses: Vec<String> = sqlx::query_scalar(
        "SELECT status FROM runs WHERE conversation_id = 'parent-conversation' ORDER BY created_at_ms",
    )
    .fetch_all(store.pool())
    .await
    .unwrap();
    assert_eq!(statuses, ["cancelled", "completed"]);
}

#[tokio::test]
async fn partially_covered_batch_only_follows_up_the_uncovered_completion() {
    let (_directory, store) = temp_store().await;
    let provider = FakeProvider::default();
    provider.push(text_response("model-call-1", "followed up a"));
    let registry = registry(store.clone(), provider.clone());
    let first = registry.get_or_create("partial-coverage-1").await.unwrap();
    let (checkpoint, _) = drive_completion(
        &first,
        completion_run(
            "child-a",
            "partial-run-1",
            pb::ConversationStateStructure::default(),
        ),
    )
    .await;

    // [A, B] 批次里 A 的通知与总结都已提交,只有 B 允许触发新的 follow-up。
    provider.push(text_response("model-call-2", "followed up b"));
    let second = registry.get_or_create("partial-coverage-2").await.unwrap();
    drive_completion(
        &second,
        batch_completion_run(
            &[("child-a", "task-call"), ("child-b", "task-call-b")],
            "partial-run-2",
            checkpoint,
        ),
    )
    .await;

    let requests = provider.requests();
    assert_eq!(
        requests.len(),
        2,
        "only the uncovered completion may activate the model"
    );
    let runtime_ids = requests[1]
        .history
        .iter()
        .map(|message| message.message_id.as_str())
        .filter(|id| id.starts_with("runtime:"))
        .collect::<Vec<_>>();
    assert_eq!(
        runtime_ids,
        [
            "runtime:background-completed:BACKGROUND_TASK_KIND_SUBAGENT:child-a:task-call",
            "runtime:background-completed:BACKGROUND_TASK_KIND_SUBAGENT:child-b:task-call-b",
        ]
    );
    let messages = store
        .load_current_messages(&cursor_server::model::ConversationId::new(
            "parent-conversation",
        ))
        .await
        .unwrap();
    for event_id in [
        "background-completed:BACKGROUND_TASK_KIND_SUBAGENT:child-a:task-call",
        "background-completed:BACKGROUND_TASK_KIND_SUBAGENT:child-b:task-call-b",
    ] {
        assert_eq!(
            messages
                .iter()
                .filter(|message| message.runtime_event_id.as_deref() == Some(event_id))
                .count(),
            1,
            "{event_id} must be committed exactly once"
        );
    }
}

#[tokio::test]
async fn back_to_back_duplicate_delivery_activates_the_model_once() {
    let (_directory, store) = temp_store().await;
    let provider = FakeProvider::default();
    let release = provider.push_gated(text_response("model-call", "followed up"));
    provider.push(text_response(
        "model-call-2",
        "unexpected second activation",
    ));
    let registry = registry(store.clone(), provider.clone());
    let first = registry.get_or_create("dup-delivery-1").await.unwrap();
    let first_run = tokio::spawn(async move {
        drive_completion(
            &first,
            completion_run(
                "dup-child",
                "dup-run-1",
                pb::ConversationStateStructure::default(),
            ),
        )
        .await
    });
    let second = registry.get_or_create("dup-delivery-2").await.unwrap();
    let mut wait_output = second.subscribe();
    let mut wait_seqno = 0;
    wait_for_provider_requests(&provider, &second, &mut wait_output, &mut wait_seqno, 1).await;

    // 同一通知的背靠背重投(客户端 state 尚未包含通知):并入活动 Run,重复批被去重。
    let second_run = tokio::spawn(async move {
        drive_forwarded_completion(
            &second,
            completion_run(
                "dup-child",
                "dup-run-2",
                pb::ConversationStateStructure::default(),
            ),
        )
        .await
    });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    release.notify_one();

    second_run.await.unwrap();
    first_run.await.unwrap();
    assert_eq!(
        provider.requests().len(),
        1,
        "duplicate delivery must not reactivate the model"
    );
    let run_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM runs WHERE conversation_id = 'parent-conversation'",
    )
    .fetch_one(store.pool())
    .await
    .unwrap();
    assert_eq!(run_count, 1);
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
                message.runtime_event_id.as_deref()
                    == Some(
                        "background-completed:BACKGROUND_TASK_KIND_SUBAGENT:dup-child:task-call",
                    )
            })
            .count(),
        1
    );
}

#[tokio::test]
async fn completion_already_consumed_by_await_does_not_start_another_parent_turn() {
    let (_directory, store) = temp_store().await;
    let provider = FakeProvider::default();
    let registry = registry(store.clone(), provider.clone());
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
        assert_eq!(exec.id, 0);
        assert!(matches!(
            exec.message,
            Some(pb::exec_server_message::Message::RequestContextArgs(_))
        ));
        vec![stream_close(0), request_context_success(0)]
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
        assert_eq!(exec.id, 0);
        vec![stream_close(0), request_context_success(0)]
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
    pb::AgentClientMessage {
        message: Some(pb::agent_client_message::Message::RunRequest(
            pb::AgentRunRequest {
                action: Some(pb::ConversationAction {
                    action: Some(
                        pb::conversation_action::Action::BackgroundTaskCompletionAction(
                            pb::BackgroundTaskCompletionAction {
                                completions: vec![pb::BackgroundTaskCompletion {
                                    task_id: child_id.into(),
                                    kind: pb::BackgroundTaskKind::Subagent as i32,
                                    status: pb::BackgroundTaskStatus::Success as i32,
                                    title: "Inspect protocol".into(),
                                    detail: Some(detail.into()),
                                    output_path: Some("/tmp/child.jsonl".into()),
                                    reason: pb::BackgroundTaskCompletionReason::TaskFinished as i32,
                                    subagent_id: Some(child_id.into()),
                                    tool_call_id: Some("task-call".into()),
                                    ..Default::default()
                                }],
                            },
                        ),
                    ),
                    ..Default::default()
                }),
                conversation_id: Some("parent-conversation".into()),
                requested_model: Some(pb::RequestedModel {
                    model_id: "test-model".into(),
                    ..Default::default()
                }),
                conversation_state: Some(conversation_state),
                run_id: Some(run_id.into()),
                ..Default::default()
            },
        )),
    }
}

fn batch_completion_run(
    completions: &[(&str, &str)],
    run_id: &str,
    conversation_state: pb::ConversationStateStructure,
) -> pb::AgentClientMessage {
    let completions = completions
        .iter()
        .map(|(child_id, tool_call_id)| pb::BackgroundTaskCompletion {
            task_id: (*child_id).into(),
            kind: pb::BackgroundTaskKind::Subagent as i32,
            status: pb::BackgroundTaskStatus::Success as i32,
            title: "Inspect protocol".into(),
            detail: Some(format!("{child_id} result")),
            reason: pb::BackgroundTaskCompletionReason::TaskFinished as i32,
            subagent_id: Some((*child_id).into()),
            tool_call_id: Some((*tool_call_id).into()),
            ..Default::default()
        })
        .collect();
    pb::AgentClientMessage {
        message: Some(pb::agent_client_message::Message::RunRequest(
            pb::AgentRunRequest {
                action: Some(pb::ConversationAction {
                    action: Some(
                        pb::conversation_action::Action::BackgroundTaskCompletionAction(
                            pb::BackgroundTaskCompletionAction { completions },
                        ),
                    ),
                    ..Default::default()
                }),
                conversation_id: Some("parent-conversation".into()),
                requested_model: Some(pb::RequestedModel {
                    model_id: "test-model".into(),
                    ..Default::default()
                }),
                conversation_state: Some(conversation_state),
                run_id: Some(run_id.into()),
                ..Default::default()
            },
        )),
    }
}

fn cancel_action() -> pb::AgentClientMessage {
    pb::AgentClientMessage {
        message: Some(pb::agent_client_message::Message::ConversationAction(
            pb::ConversationAction {
                action: Some(pb::conversation_action::Action::CancelAction(
                    pb::CancelAction::default(),
                )),
                ..Default::default()
            },
        )),
    }
}

/// Handles one non-terminal frame: answers the request-context Exec, acks KV
/// blob writes, and collects published checkpoints.
async fn pump_frame(
    handle: &TransportHandle,
    seqno: &mut i64,
    frame: &[u8],
    checkpoints: &mut Vec<pb::ConversationStateStructure>,
) {
    let (_, payload) = connect::decode_frames(frame).unwrap().pop().unwrap();
    let server = pb::AgentServerMessage::decode(payload).unwrap();
    match server.message {
        Some(pb::agent_server_message::Message::KvServerMessage(kv)) => {
            handle
                .command(TransportCommand::Append {
                    seqno: *seqno,
                    message: Box::new(kv_ack(kv.id)),
                })
                .await
                .unwrap();
            *seqno += 1;
        }
        Some(pb::agent_server_message::Message::ExecServerMessage(exec)) => {
            assert_eq!(exec.id, 0);
            for reply in [stream_close(0), request_context_success(0)] {
                handle
                    .command(TransportCommand::Append {
                        seqno: *seqno,
                        message: Box::new(reply),
                    })
                    .await
                    .unwrap();
                *seqno += 1;
            }
        }
        Some(pb::agent_server_message::Message::ConversationCheckpointUpdate(state)) => {
            checkpoints.push(state)
        }
        _ => {}
    }
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
