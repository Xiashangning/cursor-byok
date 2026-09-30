//! Verifies TodoWrite state delivery during an active Cursor turn.
mod support;

use std::collections::HashMap;

use cursor_server::{
    cursor::{
        protocol::{connect, proto::agent::v1 as pb},
        TransportCommand, TransportHandle,
    },
    model::{ConversationId, MessageContent, ProjectedContent},
    provider::ModelEvent,
};
use prost::Message;
use serde_json::{json, Value};
use support::{
    acknowledge_kv, drive, read_success, registry, resume_action, run_request, temp_store,
    text_response, tool_calls_response, tool_response, user_message_action, FakeProvider,
};

#[tokio::test]
async fn todo_merge_and_clear_survive_checkpoint_round_trips() {
    let (_directory, store) = temp_store().await;
    let provider = FakeProvider::default();
    let registry = registry(store, provider.clone());
    let mut checkpoint = None;
    let mut blobs = HashMap::new();
    for (index, arguments) in [
        json!({"merge":false,"todos":[{"id":"a","content":"Persist me","status":"pending"}]}),
        json!({"merge":true,"todos":[{"id":"a","status":"completed"}]}),
        json!({"merge":false,"todos":[]}),
    ]
    .into_iter()
    .enumerate()
    {
        provider.push(tool_response(
            &format!("todo-model-{index}"),
            &format!("todo-{index}"),
            "TodoWrite",
            &arguments.to_string(),
        ));
        provider.push(text_response(&format!("done-{index}"), "done"));
        let request_id = format!("todo-request-{index}");
        let handle = registry.get_or_create(&request_id).await.unwrap();
        let mut output = handle.subscribe().unwrap();
        handle
            .command(TransportCommand::Append {
                seqno: 0,
                message: Box::new(run_request(
                    "todo-conversation",
                    &request_id,
                    "test-model",
                    checkpoint.take(),
                    user_message_action("update todos", &format!("todo-user-{index}"), None),
                )),
            })
            .await
            .unwrap();
        let mut seqno = 1;
        let out = drive(&handle, &mut output, &mut seqno, |_| {
            panic!("TodoWrite is server-local")
        })
        .await;
        assert_eq!(out.terminal, json!({}));
        blobs.extend(out.blobs);
        let state = out
            .checkpoints
            .iter()
            .rev()
            .find(|state| state.pending_tool_calls.is_empty())
            .unwrap()
            .clone();
        if index < 2 {
            assert_eq!(state.todos.len(), 1);
            let todo = pb::TodoItem::decode(blobs[&state.todos[0]].as_slice()).unwrap();
            assert_eq!(todo.content, "Persist me");
            assert_eq!(
                todo.status,
                if index == 0 {
                    pb::TodoStatus::Pending as i32
                } else {
                    pb::TodoStatus::Completed as i32
                }
            );
        } else {
            assert!(state.todos.is_empty());
        }
        checkpoint = Some(state);
    }
}

#[tokio::test]
async fn live_todo_checkpoint_resumes_only_remaining_tools_without_state_loss() {
    let (_directory, store) = temp_store().await;
    let provider = FakeProvider::default();
    provider.push(todo_and_read_response());
    provider.push(text_response("todo-resumed-model", "finished after resume"));
    let registry = registry(store.clone(), provider.clone());

    let first = registry.get_or_create("todo-before-resume").await.unwrap();
    let mut first_output = first.subscribe().unwrap();
    start_run(
        &first,
        "todo-resume-conversation",
        "todo-before-resume-run",
        None,
        user_message_action(
            "update the todo and inspect a file",
            "todo-before-resume-user",
            Some(pb::RequestContext::default()),
        ),
    )
    .await;
    let mut first_seqno = 1;
    let first_live = collect_live(&first, &mut first_output, &mut first_seqno, 1).await;
    assert_eq!(first_live.todo_completions, ["todo-live-call"]);
    assert!(
        !first_live.saw_turn_end,
        "Todo state must be visible while the sibling tool is still active"
    );
    let checkpoint = first_live
        .checkpoints
        .iter()
        .rev()
        .find(|state| !state.todos.is_empty())
        .expect("live Todo checkpoint")
        .clone();
    assert_eq!(
        todo_shape(&decode_todos(&checkpoint, &first_live.blobs)),
        [(
            "inspect",
            "Inspect the file",
            pb::TodoStatus::InProgress as i32
        )]
    );
    assert_eq!(pending_call_ids(&checkpoint), ["read-live-call"]);
    let completed = first_live
        .timeline
        .iter()
        .position(|event| event == "todo-completed:todo-live-call")
        .expect("TodoWrite completion event");
    let published = first_live
        .timeline
        .iter()
        .position(|event| event == "todo-state")
        .expect("Todo checkpoint event");
    assert!(completed < published);
    let live_messages = store
        .load_current_messages(&ConversationId::new("todo-resume-conversation"))
        .await
        .unwrap();
    assert!(live_messages.iter().any(|message| matches!(
        &message.content,
        MessageContent::ToolResult(result)
            if result.call_id == "todo-live-call" && !result.is_error
    )));
    first.disconnect().await;

    let resumed = registry.get_or_create("todo-after-resume").await.unwrap();
    let mut resumed_output = resumed.subscribe().unwrap();
    start_run(
        &resumed,
        "todo-resume-conversation",
        "todo-after-resume-run",
        Some(checkpoint),
        resume_action(None),
    )
    .await;
    let mut resumed_seqno = 1;
    let resumed_live = collect_live(&resumed, &mut resumed_output, &mut resumed_seqno, 0).await;
    assert_eq!(
        provider.request_count(),
        1,
        "resume must finish the remaining tool before calling the model"
    );
    assert!(resumed_live.todo_completions.is_empty());
    assert!(resumed_live
        .checkpoints
        .iter()
        .all(|state| state.todos.len() == 1));

    resumed
        .command(TransportCommand::Append {
            seqno: resumed_seqno,
            message: Box::new(read_success(
                resumed_live.read_id.expect("resumed Read exec"),
                "/tmp/live-todo.txt",
                "value",
            )),
        })
        .await
        .unwrap();
    resumed_seqno += 1;
    let out = drive(&resumed, &mut resumed_output, &mut resumed_seqno, |_| {
        panic!("only the recovered Read tool should require client execution")
    })
    .await;
    assert_eq!(out.terminal, json!({}));
    assert!(out.checkpoints.iter().all(|state| state.todos.len() == 1));

    let messages = store
        .load_current_messages(&ConversationId::new("todo-resume-conversation"))
        .await
        .unwrap();
    assert_eq!(
        messages
            .iter()
            .filter(|message| {
                matches!(
                    &message.content,
                    MessageContent::ToolResult(result)
                        if result.call_id == "todo-live-call" && !result.is_error
                )
            })
            .count(),
        1,
        "checkpoint resume must not execute TodoWrite again"
    );

    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(
        requests[0].history,
        requests[1].history[..requests[0].history.len()],
        "resumption must preserve the original provider prefix"
    );
    assert_eq!(
        requests[1]
            .history
            .iter()
            .filter(|message| {
                matches!(
                    &message.content,
                    ProjectedContent::ToolResult(result)
                        if result.call_id == "todo-live-call"
                )
            })
            .count(),
        1
    );
}

#[derive(Default)]
struct LiveOutput {
    blobs: HashMap<Vec<u8>, Vec<u8>>,
    checkpoints: Vec<pb::ConversationStateStructure>,
    todo_completions: Vec<String>,
    read_id: Option<u32>,
    saw_turn_end: bool,
    timeline: Vec<String>,
}

async fn collect_live(
    handle: &TransportHandle,
    output: &mut tokio::sync::mpsc::Receiver<bytes::Bytes>,
    seqno: &mut i64,
    expected_todo_completions: usize,
) -> LiveOutput {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    let mut out = LiveOutput::default();
    loop {
        assert!(
            tokio::time::Instant::now() < deadline,
            "live observation timed out"
        );
        let Ok(Some(frame)) =
            tokio::time::timeout(std::time::Duration::from_millis(500), output.recv()).await
        else {
            continue;
        };
        let (flags, payload) = connect::decode_frames(&frame).unwrap().pop().unwrap();
        assert_eq!(
            flags & connect::END_STREAM_FLAG,
            0,
            "active sibling tool must keep the turn open: {}",
            String::from_utf8_lossy(&payload)
        );
        let server = pb::AgentServerMessage::decode(payload).unwrap();
        match server.message {
            Some(pb::agent_server_message::Message::KvServerMessage(kv)) => {
                if let Some(pb::kv_server_message::Message::SetBlobArgs(set)) = &kv.message {
                    out.blobs.insert(set.blob_id.clone(), set.blob_data.clone());
                }
            }
            Some(pb::agent_server_message::Message::ExecServerMessage(exec)) => {
                if matches!(
                    exec.message,
                    Some(pb::exec_server_message::Message::ReadArgs(_))
                ) {
                    out.read_id = Some(exec.id);
                }
            }
            Some(pb::agent_server_message::Message::InteractionUpdate(update)) => {
                match update.message {
                    Some(pb::interaction_update::Message::ToolCallCompleted(completed))
                        if completed.call_id.starts_with("todo-") =>
                    {
                        out.timeline
                            .push(format!("todo-completed:{}", completed.call_id));
                        out.todo_completions.push(completed.call_id);
                    }
                    Some(pb::interaction_update::Message::TurnEnded(_)) => {
                        out.saw_turn_end = true;
                        out.timeline.push("turn-ended".into());
                    }
                    _ => {}
                }
            }
            Some(pb::agent_server_message::Message::ConversationCheckpointUpdate(state)) => {
                if !state.todos.is_empty() {
                    out.timeline.push("todo-state".into());
                }
                out.checkpoints.push(state);
            }
            _ => {}
        }
        acknowledge_kv(handle, seqno, &frame).await;
        if out.read_id.is_some()
            && out.todo_completions.len() >= expected_todo_completions
            && out.checkpoints.iter().any(|state| !state.todos.is_empty())
        {
            return out;
        }
    }
}

async fn start_run(
    handle: &TransportHandle,
    conversation_id: &str,
    run_id: &str,
    state: Option<pb::ConversationStateStructure>,
    action: pb::conversation_action::Action,
) {
    handle
        .command(TransportCommand::Append {
            seqno: 0,
            message: Box::new(run_request(
                conversation_id,
                run_id,
                "test-model",
                state,
                action,
            )),
        })
        .await
        .unwrap();
}

fn todo_and_read_response() -> Vec<ModelEvent> {
    tool_calls_response(
        "todo-live-model",
        &[
            (
                "todo-live-call",
                "TodoWrite",
                json!({
                    "merge": false,
                    "todos": [{
                        "id": "inspect",
                        "content": "Inspect the file",
                        "status": "in_progress"
                    }]
                })
                .to_string(),
            ),
            (
                "read-live-call",
                "Read",
                json!({"path": "/tmp/live-todo.txt"}).to_string(),
            ),
        ],
    )
}

fn decode_todos(
    state: &pb::ConversationStateStructure,
    blobs: &HashMap<Vec<u8>, Vec<u8>>,
) -> Vec<pb::TodoItem> {
    state
        .todos
        .iter()
        .map(|id| {
            pb::TodoItem::decode(
                blobs
                    .get(id)
                    .unwrap_or_else(|| panic!("Todo Blob was not delivered: {id:?}"))
                    .as_slice(),
            )
            .unwrap()
        })
        .collect()
}

fn todo_shape(todos: &[pb::TodoItem]) -> Vec<(&str, &str, i32)> {
    todos
        .iter()
        .map(|todo| (todo.id.as_str(), todo.content.as_str(), todo.status))
        .collect()
}

fn pending_call_ids(state: &pb::ConversationStateStructure) -> Vec<String> {
    let [pending] = state.pending_tool_calls.as_slice() else {
        panic!("expected exactly one pending assistant")
    };
    let value: Value = serde_json::from_str(pending).unwrap();
    value["content"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|part| part["type"] == "tool-call")
        .filter_map(|part| part["toolCallId"].as_str())
        .map(str::to_string)
        .collect()
}
