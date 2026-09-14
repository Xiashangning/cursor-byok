//! Mock IDE steering acknowledgements must preserve child identity and ownership.
mod support;

use cursor_server::cursor::{protocol::proto::agent::v1 as pb, TransportCommand};
use support::{
    drive, registry, run_request, temp_store, text_response, tool_response, user_message_action,
    FakeProvider,
};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_live_child_updates_do_not_publish_creation_cards_or_replace_owners() {
    let jobs = (0..8).map(|index| async move {
        let (_directory, store) = temp_store().await;
        let provider = FakeProvider::default();
        let child = format!("child-{index}");
        provider.push(tool_response(
            "update",
            "update-call",
            "send-message-to-agent",
            &serde_json::json!({
                "agent_id":child, "prompt":"keep working"
            })
            .to_string(),
        ));
        provider.push(text_response("done", "updated"));
        let registry = registry(store, provider);
        let handle = registry
            .get_or_create(&format!("parent-{index}"))
            .await
            .unwrap();
        let state = pb::ConversationStateStructure {
            mode: Some(pb::AgentMode::Multitask as i32),
            subagent_states: std::collections::HashMap::from([(
                child.clone(),
                pb::SubagentPersistedState {
                    model_id: Some("child-model".into()),
                    ..Default::default()
                },
            )]),
            subagent_runs_by_parent_tool_call_id: std::collections::HashMap::from([(
                "create-call".into(),
                pb::SubagentRunState {
                    parent_tool_call_id: "create-call".into(),
                    subagent_id: Some(child.clone()),
                    task_id: Some(child.clone()),
                    status: pb::SubagentRunStatus::Backgrounded as i32,
                    ..Default::default()
                },
            )]),
            ..Default::default()
        };
        let mut output = handle.subscribe();
        handle
            .command(TransportCommand::Append {
                seqno: 0,
                message: Box::new(run_request(
                    "parent",
                    "parent-run",
                    "parent-model",
                    Some(state),
                    user_message_action(
                        "update child",
                        "user",
                        Some(pb::RequestContext::default()),
                    ),
                )),
            })
            .await
            .unwrap();
        let mut seqno = 1;
        let mut exec_count = 0;
        let out = drive(&handle, &mut output, &mut seqno, |exec| {
            let Some(pb::exec_server_message::Message::SubagentArgs(args)) = &exec.message else {
                panic!("expected subagent request")
            };
            exec_count += 1;
            assert_eq!(args.resume_agent_id.as_deref(), Some(child.as_str()));
            assert_eq!(args.model_id, "child-model");
            assert_eq!(args.parent_conversation_id.as_deref(), Some("parent"));
            vec![
                pb::AgentClientMessage {
                    message: Some(pb::agent_client_message::Message::ExecClientMessage(
                        pb::ExecClientMessage {
                            id: exec.id,
                            message: Some(pb::exec_client_message::Message::SubagentResult(
                                pb::SubagentResult {
                                    result: Some(pb::subagent_result::Result::Success(
                                        pb::SubagentSuccess {
                                            agent_id: child.clone(),
                                            background_reason:
                                                pb::SubagentBackgroundReason::QueuedFollowUp as i32,
                                            ..Default::default()
                                        },
                                    )),
                                },
                            )),
                            ..Default::default()
                        },
                    )),
                },
                support::stream_close(exec.id),
            ]
        })
        .await;
        assert_eq!(out.terminal, serde_json::json!({}));
        assert_eq!(exec_count, 1);
        for interaction in &out.interactions {
            let tool = match &interaction.message {
                Some(pb::interaction_update::Message::PartialToolCall(update)) => {
                    update.tool_call.as_ref()
                }
                Some(pb::interaction_update::Message::ToolCallStarted(update)) => {
                    update.tool_call.as_ref()
                }
                Some(pb::interaction_update::Message::ToolCallCompleted(update)) => {
                    update.tool_call.as_ref()
                }
                _ => None,
            };
            if let Some(pb::tool_call::Tool::TaskToolCall(task)) =
                tool.and_then(|tool| tool.tool.as_ref())
            {
                assert_eq!(
                    task.args.as_ref().and_then(|args| args.resume.as_deref()),
                    Some(child.as_str()),
                    "an update must never publish a blank new-agent card"
                );
            }
        }
        for checkpoint in &out.checkpoints {
            let runs = &checkpoint.subagent_runs_by_parent_tool_call_id;
            assert_eq!(runs.len(), 1);
            assert!(
                runs.contains_key("create-call"),
                "queued update must preserve the live child's completion owner"
            );
        }
    });
    futures_util::future::join_all(jobs).await;
}
