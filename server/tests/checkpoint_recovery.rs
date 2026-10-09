//! Verifies Conversation recovery and resumable checkpoint state.
mod support;

use std::collections::HashSet;

use cursor_server::{
    cursor::{
        protocol::{connect, proto::agent::v1 as pb},
        TransportCommand,
    },
    model::ToolRoundId,
    store::BlobId,
};
use prost::Message;
use support::{
    kv_ack, read_success, registry, resume_action, run_request, temp_store, text_response,
    tool_response, user_message_action, FakeProvider,
};

#[tokio::test]
async fn eligible_pending_checkpoint_resumes_tools_before_the_next_model_call() {
    let (_directory, store) = temp_store().await;
    let provider = FakeProvider::default();
    provider.push(tool_response(
        "model-1",
        "read-1",
        "Read",
        "{\"path\":\"/tmp/a\"}",
    ));
    provider.push(text_response("model-2", "resumed"));
    let registry = registry(store.clone(), provider.clone());

    let first = registry.get_or_create("first-run").await.unwrap();
    let mut first_output = first.subscribe().unwrap();
    first
        .command(TransportCommand::Append {
            seqno: 0,
            message: Box::new(run_request(
                "conversation",
                "first-run",
                "test-model",
                None,
                user_message_action("read", "user-1", None),
            )),
        })
        .await
        .unwrap();
    let mut first_seqno = 1;
    let mut sent_blob_ids = HashSet::new();
    let staged = loop {
        let server = next_message(&mut first_output).await;
        match server.message {
            Some(pb::agent_server_message::Message::KvServerMessage(kv)) => {
                if let Some(pb::kv_server_message::Message::SetBlobArgs(args)) = &kv.message {
                    sent_blob_ids.insert(args.blob_id.clone());
                }
                acknowledge(&first, &mut first_seqno, kv.id).await;
            }
            Some(pb::agent_server_message::Message::ConversationCheckpointUpdate(state))
                if state.pending_tool_calls.len() == 1 =>
            {
                break state;
            }
            _ => {}
        }
    };
    assert!(
        !sent_blob_ids.contains(
            BlobId::digest(&staged.encode_to_vec())
                .as_bytes()
                .as_slice()
        ),
        "ConversationStateStructure is inline and must not be sent as a Blob"
    );
    let staged_started_at_ms =
        serde_json::from_str::<serde_json::Value>(staged.pending_tool_calls.first().unwrap())
            .unwrap()["providerOptions"]["cursor"]["pendingToolCallStartedAtMs"]
            .as_u64()
            .unwrap();
    assert_eq!(provider.requests().len(), 1);
    first.disconnect().await;

    let resumed = registry.get_or_create("resumed-run").await.unwrap();
    let mut resumed_output = resumed.subscribe().unwrap();
    let mut resumed_checkpoints = Vec::new();
    let mut resumed_set_blob_ids = HashSet::new();
    resumed
        .command(TransportCommand::Append {
            seqno: 0,
            message: Box::new(run_request(
                "conversation",
                "resumed-run",
                "test-model",
                Some(staged.clone()),
                resume_action(None),
            )),
        })
        .await
        .unwrap();
    let mut resumed_seqno = 1;
    let exec_id = loop {
        let server = next_message(&mut resumed_output).await;
        match server.message {
            Some(pb::agent_server_message::Message::KvServerMessage(kv)) => {
                if let Some(pb::kv_server_message::Message::SetBlobArgs(args)) = &kv.message {
                    resumed_set_blob_ids.insert(args.blob_id.clone());
                }
                acknowledge(&resumed, &mut resumed_seqno, kv.id).await;
            }
            Some(pb::agent_server_message::Message::ConversationCheckpointUpdate(state)) => {
                resumed_checkpoints.push(state);
            }
            Some(pb::agent_server_message::Message::ExecServerMessage(exec)) => break exec.id,
            _ => {}
        }
    };
    assert_eq!(
        provider.requests().len(),
        1,
        "resume must execute the pending batch before calling the model"
    );
    let resumed_run_id = store
        .active_run_for_cursor_request("resumed-run")
        .await
        .unwrap()
        .unwrap();
    let resumed_round = store
        .tool_round(&ToolRoundId::new(format!(
            "{}:round:resume",
            resumed_run_id.as_str()
        )))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(resumed_round.created_at_ms, staged_started_at_ms);
    resumed
        .command(TransportCommand::Append {
            seqno: resumed_seqno,
            message: Box::new(read_success(exec_id, "/tmp/a", "value")),
        })
        .await
        .unwrap();
    resumed_seqno += 1;

    let mut saw_settled_barrier_blob = false;
    let mut saw_settled_checkpoint = false;
    loop {
        let frame = tokio::time::timeout(std::time::Duration::from_secs(5), resumed_output.recv())
            .await
            .unwrap()
            .unwrap();
        let (flags, payload) = connect::decode_frames(&frame).unwrap().pop().unwrap();
        if flags & connect::END_STREAM_FLAG != 0 {
            break;
        }
        let server = pb::AgentServerMessage::decode(payload).unwrap();
        match server.message {
            Some(pb::agent_server_message::Message::KvServerMessage(kv)) => {
                if let Some(pb::kv_server_message::Message::SetBlobArgs(args)) = &kv.message {
                    resumed_set_blob_ids.insert(args.blob_id.clone());
                }
                if !saw_settled_barrier_blob {
                    assert_eq!(
                        provider.requests().len(),
                        1,
                        "the next model call must wait for the settled Blob barrier"
                    );
                    saw_settled_barrier_blob = true;
                }
                acknowledge(&resumed, &mut resumed_seqno, kv.id).await;
            }
            Some(pb::agent_server_message::Message::ConversationCheckpointUpdate(state)) => {
                if state.pending_tool_calls.is_empty() {
                    saw_settled_checkpoint = true;
                }
                resumed_checkpoints.push(state);
            }
            Some(pb::agent_server_message::Message::InteractionUpdate(update))
                if matches!(
                    update.message,
                    Some(pb::interaction_update::Message::TextDelta(_))
                ) =>
            {
                assert!(
                    saw_settled_checkpoint,
                    "the next model round must not become visible before settled checkpoint"
                );
            }
            _ => {}
        }
    }
    assert!(saw_settled_barrier_blob);
    assert!(saw_settled_checkpoint);
    assert!(staged
        .root_prompt_messages_json
        .iter()
        .all(|id| !resumed_set_blob_ids.contains(id)));
    assert_eq!(provider.requests().len(), 2);
    assert!(resumed_checkpoints
        .last()
        .unwrap()
        .read_paths
        .iter()
        .any(|path| path == "/tmp/a"));

    let mut previous_steps = Vec::new();
    let mut saw_completed_read = false;
    for state in resumed_checkpoints {
        let Some(turn_id) = state.turns.last() else {
            continue;
        };
        let turn = pb::ConversationTurnStructure::decode(
            store
                .get_blob(&BlobId::from_bytes(turn_id).unwrap())
                .await
                .unwrap()
                .unwrap()
                .as_slice(),
        )
        .unwrap();
        let pb::conversation_turn_structure::Turn::AgentConversationTurn(turn) = turn.turn.unwrap()
        else {
            panic!("expected agent turn");
        };
        assert!(turn.steps.len() >= previous_steps.len());
        assert_eq!(
            previous_steps,
            turn.steps[..previous_steps.len()],
            "published Step BlobIDs must be an immutable prefix"
        );
        previous_steps = turn.steps.clone();
        for step_id in &turn.steps {
            let step = pb::ConversationStep::decode(
                store
                    .get_blob(&BlobId::from_bytes(step_id).unwrap())
                    .await
                    .unwrap()
                    .unwrap()
                    .as_slice(),
            )
            .unwrap();
            if let Some(pb::conversation_step::Message::ToolCall(call)) = step.message {
                if call.tool_call_id.as_deref() == Some("read-1") {
                    assert!(call.started_at_ms.is_some());
                    assert!(call.completed_at_ms.is_some());
                    saw_completed_read = true;
                }
            }
        }
    }
    assert!(
        saw_completed_read,
        "settled Turn must keep the typed result"
    );
}

#[tokio::test]
async fn recovery_rejects_a_kv_get_payload_whose_hash_does_not_match_the_blob_id() {
    let (_directory, store) = temp_store().await;
    let registry = registry(store, FakeProvider::default());
    let handle = registry.get_or_create("bad-blob-run").await.unwrap();
    let mut output = handle.subscribe().unwrap();
    let expected = BlobId::digest(b"expected");
    handle
        .command(TransportCommand::Append {
            seqno: 0,
            message: Box::new(run_request(
                "conversation",
                "resumed-run",
                "test-model",
                Some(pb::ConversationStateStructure {
                    root_prompt_messages_json: vec![expected.as_bytes().to_vec()],
                    mode: Some(pb::AgentMode::Agent as i32),
                    ..Default::default()
                }),
                resume_action(None),
            )),
        })
        .await
        .unwrap();

    let get_id = loop {
        let server = next_message(&mut output).await;
        if let Some(pb::agent_server_message::Message::KvServerMessage(kv)) = server.message {
            if matches!(
                kv.message,
                Some(pb::kv_server_message::Message::GetBlobArgs(_))
            ) {
                break kv.id;
            }
        }
    };
    handle
        .command(TransportCommand::Append {
            seqno: 1,
            message: Box::new(pb::AgentClientMessage {
                message: Some(pb::agent_client_message::Message::KvClientMessage(
                    pb::KvClientMessage {
                        id: get_id,
                        message: Some(pb::kv_client_message::Message::GetBlobResult(
                            pb::GetBlobResult {
                                blob_data: Some(b"corrupt".to_vec()),
                                error: None,
                            },
                        )),
                    },
                )),
            }),
        })
        .await
        .unwrap();

    let error = loop {
        let frame = tokio::time::timeout(std::time::Duration::from_secs(5), output.recv())
            .await
            .unwrap()
            .unwrap();
        let (flags, payload) = connect::decode_frames(&frame).unwrap().pop().unwrap();
        if flags & connect::END_STREAM_FLAG != 0 {
            break serde_json::from_slice::<serde_json::Value>(&payload).unwrap();
        }
    };
    assert_eq!(error["error"]["code"], "invalid_argument");
    assert!(error["error"]["message"]
        .as_str()
        .unwrap()
        .contains("Blob hash mismatch"));
}

async fn next_message(
    output: &mut tokio::sync::mpsc::Receiver<bytes::Bytes>,
) -> pb::AgentServerMessage {
    let frame = tokio::time::timeout(std::time::Duration::from_secs(5), output.recv())
        .await
        .unwrap()
        .unwrap();
    let (flags, payload) = connect::decode_frames(&frame).unwrap().pop().unwrap();
    assert_eq!(
        flags & connect::END_STREAM_FLAG,
        0,
        "unexpected EndStream: {}",
        String::from_utf8_lossy(&payload)
    );
    pb::AgentServerMessage::decode(payload).unwrap()
}

async fn acknowledge(handle: &cursor_server::cursor::TransportHandle, seqno: &mut i64, id: u32) {
    handle
        .command(TransportCommand::Append {
            seqno: *seqno,
            message: Box::new(kv_ack(id)),
        })
        .await
        .unwrap();
    *seqno += 1;
}
