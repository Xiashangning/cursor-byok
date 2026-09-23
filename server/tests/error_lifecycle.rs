//! Verifies unique persisted and streamed terminal outcomes.
mod support;

use axum::http::StatusCode;
use base64::{engine::general_purpose::STANDARD_NO_PAD, Engine};
use cursor_server::{
    cursor::protocol::{
        connect,
        proto::{agent::v1 as pb, aiserver::v1 as ai},
    },
    cursor::{TransportCommand, TransportParent},
    model::{MessageContent, Role},
    provider::{FinishReason, ModelEvent},
    Error,
};
use prost::Message;
use support::{
    acknowledge_kv, drive, read_success, registry, run_request, temp_store, text_response,
    tool_response, user_message_action, FakeProvider,
};

#[tokio::test]
async fn abort_command_cancels_the_run_and_closes_output() {
    let (_directory, store) = temp_store().await;
    let registry = registry(store, FakeProvider::default());
    let handle = registry.get_or_create("abort-request").await.unwrap();
    let mut output = handle.subscribe().unwrap();

    handle.command(TransportCommand::Disconnect).await.unwrap();

    let frame = tokio::time::timeout(std::time::Duration::from_secs(1), output.recv())
        .await
        .unwrap()
        .expect("Abort must emit a terminal frame");
    let (flags, payload) = connect::decode_frames(&frame).unwrap().pop().unwrap();
    assert_eq!(flags, connect::END_STREAM_FLAG);
    let payload: serde_json::Value = serde_json::from_slice(&payload).unwrap();
    assert_eq!(payload["error"]["code"], "canceled");
    assert_eq!(output.recv().await, None);
}

#[tokio::test]
async fn provider_failure_retries_from_the_current_checkpoint_without_hiding_partial_output() {
    let (_directory, store) = temp_store().await;
    let provider = FakeProvider::default();
    provider.push_results(vec![
        Ok(ModelEvent::Start {
            model_call_id: "attempt-0".into(),
        }),
        Ok(ModelEvent::TextStart),
        Ok(ModelEvent::TextDelta("partial ".into())),
        Ok(ModelEvent::ToolCallStart {
            index: 0,
            call_id: "failed-tool".into(),
            name: "Read".into(),
        }),
        Ok(ModelEvent::ToolCallArgumentsDelta {
            index: 0,
            delta: "{\"path\":".into(),
        }),
        Err(Error::Provider("stream disconnected".into())),
    ]);
    provider.push(text_response("attempt-1", "completed"));
    let registry = registry(store.clone(), provider.clone());
    let handle = registry.get_or_create("retry-request").await.unwrap();
    let mut output = handle.subscribe().unwrap();
    handle
        .command(TransportCommand::Append {
            seqno: 0,
            message: Box::new(protocol_client_run("retry", "retry-user")),
        })
        .await
        .unwrap();

    let mut seqno = 1;
    let mut text = String::new();
    let mut failed_tool_completed = false;
    loop {
        let frame = tokio::time::timeout(std::time::Duration::from_secs(60), output.recv())
            .await
            .unwrap()
            .unwrap();
        let (flags, payload) = connect::decode_frames(&frame).unwrap().pop().unwrap();
        if flags & connect::END_STREAM_FLAG != 0 {
            assert_eq!(
                serde_json::from_slice::<serde_json::Value>(&payload).unwrap(),
                serde_json::json!({})
            );
            break;
        }
        let server = pb::AgentServerMessage::decode(payload).unwrap();
        match server.message {
            Some(pb::agent_server_message::Message::KvServerMessage(_)) => {
                acknowledge_kv(&handle, &mut seqno, &frame).await;
            }
            Some(pb::agent_server_message::Message::InteractionUpdate(update)) => {
                match update.message {
                    Some(pb::interaction_update::Message::TextDelta(delta)) => {
                        text.push_str(&delta.text);
                    }
                    Some(pb::interaction_update::Message::ToolCallCompleted(completed))
                        if completed.call_id == "failed-tool" =>
                    {
                        let tool = completed.tool_call.expect("failed tool completion");
                        failed_tool_completed = tool.completed_at_ms.is_some();
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    assert_eq!(text, "partial completed");
    assert!(
        failed_tool_completed,
        "failed attempt must terminate its partial tool card"
    );
    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0], requests[1]);
    let messages = store
        .load_current_messages(&cursor_server::model::ConversationId::new(
            "protocol-failed-conversation",
        ))
        .await
        .unwrap();
    assert!(messages.iter().any(|message| {
        matches!(&message.content, MessageContent::Assistant { text, .. } if text == "completed")
    }));
    assert!(!messages.iter().any(|message| {
        matches!(&message.content, MessageContent::Assistant { text, .. } if text.contains("partial"))
    }));
}

#[tokio::test]
async fn provider_failure_keeps_the_initial_checkpoint_then_returns_structured_error() {
    let (_directory, store) = temp_store().await;
    let provider = FakeProvider::default();
    provider.push(vec![
        ModelEvent::Start {
            model_call_id: "length-limited".into(),
        },
        ModelEvent::Done(FinishReason::Length),
    ]);
    let registry = registry(store.clone(), provider.clone());
    let handle = registry.get_or_create("failed-request").await.unwrap();
    let mut output = handle.subscribe().unwrap();
    handle
        .command(TransportCommand::Append {
            seqno: 0,
            message: Box::new(client_run()),
        })
        .await
        .unwrap();

    let mut append_seqno = 1;
    let out = drive(&handle, &mut output, &mut append_seqno, |_| vec![]).await;
    let checkpoints = &out.checkpoints;
    let saw_turn_ended = out.interactions.iter().any(|update| {
        matches!(
            update.message,
            Some(pb::interaction_update::Message::TurnEnded(_))
        )
    });
    for update in &out.interactions {
        if let Some(pb::interaction_update::Message::TextDelta(delta)) = &update.message {
            assert!(!delta.text.contains("Cursor server error"));
        }
    }
    let error_json = out.terminal;

    assert_eq!(
        checkpoints.len(),
        1,
        "the initial user state is checkpointed"
    );
    assert!(checkpoints[0].pending_tool_calls.is_empty());
    assert!(!saw_turn_ended);
    assert_eq!(error_json["error"]["code"], "unavailable");
    let detail = &error_json["error"]["details"][0];
    assert_eq!(detail["type"], "aiserver.v1.ErrorDetails");
    let encoded = detail["value"].as_str().unwrap();
    assert!(!encoded.ends_with('='));
    let decoded = STANDARD_NO_PAD.decode(encoded).unwrap();
    let decoded = ai::ErrorDetails::decode(decoded.as_slice()).unwrap();
    assert_eq!(
        decoded.error,
        ai::error_details::Error::ProviderError as i32
    );
    assert_eq!(decoded.is_expected, Some(true));
    let custom = decoded.details.unwrap();
    assert_eq!(custom.title, "Provider Error");
    assert_eq!(custom.is_retryable, Some(true));
    assert_eq!(custom.should_show_immediate_error, Some(false));
    assert_eq!(
        tokio::time::timeout(std::time::Duration::from_secs(1), output.recv())
            .await
            .unwrap(),
        None
    );

    assert_eq!(provider.requests().len(), 1, "Length must not retry");
    let messages = store
        .load_current_messages(&cursor_server::model::ConversationId::new(
            "failed-conversation",
        ))
        .await
        .unwrap();
    assert!(messages.iter().any(|message| message.role == Role::User));
    assert!(!messages.iter().any(|message| {
        matches!(
            &message.content,
            MessageContent::Assistant { text, .. } if text.contains("Cursor server error")
        )
    }));
}

#[tokio::test]
async fn rejected_provider_request_is_reported_after_one_attempt() {
    let (_directory, store) = temp_store().await;
    let provider = FakeProvider::default();
    provider.push_error(Error::ProviderStatus {
        status: StatusCode::UNAUTHORIZED,
        message: "OpenAI Chat 401 Unauthorized: invalid api key".into(),
    });
    // A retry would consume this response and finish the run successfully.
    provider.push(text_response("retried", "retried"));
    let registry = registry(store, provider.clone());
    let handle = registry.get_or_create("failed-request").await.unwrap();
    let mut output = handle.subscribe().unwrap();
    handle
        .command(TransportCommand::Append {
            seqno: 0,
            message: Box::new(client_run()),
        })
        .await
        .unwrap();

    let mut append_seqno = 1;
    let error_json = drive(&handle, &mut output, &mut append_seqno, |_| vec![])
        .await
        .terminal;

    assert_eq!(
        provider.requests().len(),
        1,
        "a 401 must be reported after one attempt"
    );
    assert_eq!(error_json["error"]["code"], "unavailable");
    let encoded = error_json["error"]["details"][0]["value"].as_str().unwrap();
    let decoded = STANDARD_NO_PAD.decode(encoded).unwrap();
    let decoded = ai::ErrorDetails::decode(decoded.as_slice()).unwrap();
    let custom = decoded.details.unwrap();
    assert!(
        custom.detail.contains("401 Unauthorized"),
        "{}",
        custom.detail
    );
}

#[tokio::test]
async fn unknown_tool_response_id_is_ignored_and_the_run_continues() {
    let (_directory, store) = temp_store().await;
    let provider = FakeProvider::default();
    provider.push(tool_response(
        "model-call",
        "call-1",
        "Read",
        "{\"path\":\"/tmp/a\"}",
    ));
    provider.push(text_response("model-call-2", "done"));
    let registry = registry(store.clone(), provider);
    let handle = registry
        .get_or_create("protocol-failed-request")
        .await
        .unwrap();
    let mut output = handle.subscribe().unwrap();
    handle
        .command(TransportCommand::Append {
            seqno: 0,
            message: Box::new(protocol_client_run("read it", "protocol-failed-user")),
        })
        .await
        .unwrap();

    let mut append_seqno = 1;
    let out = drive(&handle, &mut output, &mut append_seqno, |exec| {
        // Unknown bridge ids are ignored; the valid response still completes the tool.
        vec![
            pb::AgentClientMessage {
                message: Some(pb::agent_client_message::Message::ExecClientMessage(
                    pb::ExecClientMessage {
                        id: exec.id + 1_000,
                        exec_id: String::new(),
                        message: None,
                        ..Default::default()
                    },
                )),
            },
            read_success(exec.id, "/tmp/a", "value"),
        ]
    })
    .await;
    let saw_turn_ended = out.interactions.iter().any(|update| {
        matches!(
            update.message,
            Some(pb::interaction_update::Message::TurnEnded(_))
        )
    });
    for update in &out.interactions {
        if let Some(pb::interaction_update::Message::TextDelta(delta)) = &update.message {
            assert!(!delta.text.contains("unknown tool result"));
            assert!(!delta.text.contains("protocol error"));
        }
    }
    let end_stream = out.terminal;

    assert!(saw_turn_ended);
    assert_eq!(end_stream, serde_json::json!({}));
    assert_eq!(
        tokio::time::timeout(std::time::Duration::from_secs(1), output.recv())
            .await
            .unwrap(),
        None
    );

    let (status, failure_summary): (String, Option<String>) =
        sqlx::query_as("SELECT status, failure_summary FROM runs WHERE cursor_request_id = ?")
            .bind("protocol-failed-request")
            .fetch_one(store.pool())
            .await
            .unwrap();
    assert_eq!(status, "completed");
    assert_eq!(failure_summary, None);
}

#[tokio::test]
async fn newer_run_request_on_one_bidi_stream_replaces_the_active_run() {
    let (_directory, store) = temp_store().await;
    let provider = FakeProvider::default();
    provider.push(tool_response(
        "model-call",
        "call-1",
        "Read",
        "{\"path\":\"/tmp/a\"}",
    ));
    provider.push(text_response(
        "replacement-model-call",
        "replacement completed",
    ));
    let registry = registry(store.clone(), provider.clone());
    let handle = registry
        .get_or_create("protocol-failed-request")
        .await
        .unwrap();
    let mut output = handle.subscribe().unwrap();
    handle
        .command(TransportCommand::Append {
            seqno: 0,
            message: Box::new(protocol_client_run("read it", "first-user")),
        })
        .await
        .unwrap();

    let mut seqno = 1;
    let mut replacement_sent = false;
    let mut late_result_sent = false;
    let mut saw_abort = false;
    let mut cropped_state = None;
    let terminal_json = loop {
        let frame = tokio::time::timeout(std::time::Duration::from_secs(5), output.recv())
            .await
            .unwrap()
            .expect("RunSSE closed before Error EndStream");
        let (flags, payload) = connect::decode_frames(&frame).unwrap().pop().unwrap();
        if flags & connect::END_STREAM_FLAG != 0 {
            break serde_json::from_slice::<serde_json::Value>(&payload).unwrap();
        }
        let server = pb::AgentServerMessage::decode(payload).unwrap();
        match server.message {
            Some(pb::agent_server_message::Message::KvServerMessage(_)) => {
                acknowledge_kv(&handle, &mut seqno, &frame).await;
            }
            Some(pb::agent_server_message::Message::ConversationCheckpointUpdate(mut state)) => {
                state.root_prompt_messages_json.truncate(1);
                state.turns.clear();
                state.pending_tool_calls.clear();
                cropped_state = Some(state);
            }
            Some(pb::agent_server_message::Message::ExecServerMessage(exec))
                if !replacement_sent =>
            {
                replacement_sent = true;
                let mut replacement =
                    protocol_client_run("use the cropped history", "replacement-user");
                let Some(pb::agent_client_message::Message::RunRequest(request)) =
                    replacement.message.as_mut()
                else {
                    unreachable!()
                };
                request.conversation_state = Some(
                    cropped_state
                        .clone()
                        .expect("first Run must publish a checkpoint before tools"),
                );
                handle
                    .command(TransportCommand::Append {
                        seqno,
                        message: Box::new(replacement),
                    })
                    .await
                    .unwrap();
                seqno += 1;
                handle
                    .command(TransportCommand::Append {
                        seqno,
                        message: Box::new(pb::AgentClientMessage {
                            message: Some(pb::agent_client_message::Message::ExecClientMessage(
                                pb::ExecClientMessage {
                                    id: exec.id,
                                    message: Some(pb::exec_client_message::Message::ReadResult(
                                        pb::ReadResult::default(),
                                    )),
                                    ..Default::default()
                                },
                            )),
                        }),
                    })
                    .await
                    .unwrap();
                seqno += 1;
                late_result_sent = true;
            }
            Some(pb::agent_server_message::Message::ExecServerControlMessage(control)) => {
                if matches!(
                    control.message,
                    Some(pb::exec_server_control_message::Message::Abort(_))
                ) {
                    saw_abort = true;
                }
            }
            _ => {}
        }
    };

    assert!(replacement_sent);
    assert!(late_result_sent);
    assert!(saw_abort);
    assert!(terminal_json.get("error").is_none(), "{terminal_json}");
    assert_eq!(provider.requests().len(), 2);
    let replacement_history = serde_json::to_string(&provider.requests()[1].history).unwrap();
    assert!(replacement_history.contains("use the cropped history"));
    assert!(!replacement_history.contains("read it"));
    let statuses: Vec<String> = sqlx::query_scalar(
        "SELECT status FROM runs WHERE cursor_request_id = ? ORDER BY created_at_ms, run_id",
    )
    .bind("protocol-failed-request")
    .fetch_all(store.pool())
    .await
    .unwrap();
    assert_eq!(statuses, ["cancelled", "completed"]);
}

#[tokio::test]
async fn parent_request_does_not_need_to_resolve_to_an_active_run() {
    assert_run_starts_without_parent_dependency(
        "finished-parent-request",
        Some(TransportParent {
            request_id: "already-finished-parent".into(),
            tool_call_id: "original-tool-call".into(),
        }),
        None,
    )
    .await;
}

#[tokio::test]
async fn subagent_type_does_not_require_parent_metadata() {
    assert_run_starts_without_parent_dependency(
        "parentless-subagent-request",
        None,
        Some("generalPurpose"),
    )
    .await;
}

async fn assert_run_starts_without_parent_dependency(
    request_id: &str,
    parent: Option<TransportParent>,
    subagent_type_name: Option<&str>,
) {
    let (_directory, store) = temp_store().await;
    let provider = FakeProvider::default();
    provider.push(text_response(
        "independent-model-call",
        "continued independently",
    ));
    let registry = registry(store.clone(), provider.clone());
    let handle = registry.get_or_create(request_id).await.unwrap();
    if let Some(parent) = parent {
        handle.set_parent(parent).unwrap();
    }
    let mut output = handle.subscribe().unwrap();
    let mut message = protocol_client_run("continue", "independent-user");
    let Some(pb::agent_client_message::Message::RunRequest(request)) = message.message.as_mut()
    else {
        unreachable!()
    };
    request.conversation_id = Some(format!("{request_id}-conversation"));
    request.subagent_type_name = subagent_type_name.map(str::to_owned);
    handle
        .command(TransportCommand::Append {
            seqno: 0,
            message: Box::new(message),
        })
        .await
        .unwrap();

    let mut seqno = 1;
    let terminal_json = drive(&handle, &mut output, &mut seqno, |_| vec![])
        .await
        .terminal;

    assert!(terminal_json.get("error").is_none(), "{terminal_json}");
    assert_eq!(provider.requests().len(), 1);
    let row: (String, Option<String>, Option<String>) = sqlx::query_as(
        "SELECT run_kind, parent_run_id, parent_tool_call_id FROM runs WHERE cursor_request_id = ?",
    )
    .bind(request_id)
    .fetch_one(store.pool())
    .await
    .unwrap();
    assert_eq!(row, ("root".into(), None, None));
}

fn client_run() -> pb::AgentClientMessage {
    run_request(
        "failed-conversation",
        "failed-request",
        "test-model",
        None,
        user_message_action("hello", "failed-user", None),
    )
}

fn protocol_client_run(text: &str, message_id: &str) -> pb::AgentClientMessage {
    run_request(
        "protocol-failed-conversation",
        "protocol-failed-request",
        "test-model",
        None,
        user_message_action(text, message_id, None),
    )
}
