//! Parentage is persisted from the original Task dispatch, not the active parent run.
mod support;

use cursor_server::{
    cursor::{protocol::proto::agent::v1 as pb, TransportCommand, TransportParent},
    store::Store,
};
use support::*;

type RunIdentity = (String, Option<String>, Option<String>, Option<String>);

async fn identity(store: &Store, request_id: &str) -> RunIdentity {
    sqlx::query_as(
        "SELECT run_kind, parent_run_id, parent_tool_call_id, subagent_kind
         FROM runs WHERE cursor_request_id = ?",
    )
    .bind(request_id)
    .fetch_one(store.pool())
    .await
    .unwrap()
}

#[tokio::test]
async fn foreground_and_background_children_keep_the_dispatching_run_after_parent_finishes() {
    for background in [false, true] {
        let (_directory, store) = temp_store().await;
        let provider = FakeProvider::default();
        provider.push(tool_response(
            "parent-model-call",
            "task-call",
            "Task",
            &serde_json::json!({
                "prompt": "work", "description": "Child", "subagent_type": "explore",
                "run_in_background": background,
            })
            .to_string(),
        ));
        provider.push(text_response("parent-answer", "done"));
        let parent_registry = registry(store.clone(), provider.clone());
        let parent = parent_registry.get_or_create("parent").await.unwrap();
        let mut output = parent.subscribe().unwrap();
        parent
            .command(TransportCommand::Append {
                seqno: 0,
                message: Box::new(run_request(
                    "parent-conversation",
                    "parent",
                    "test-model",
                    None,
                    user_message_action(
                        "delegate",
                        "parent-user",
                        Some(pb::RequestContext::default()),
                    ),
                )),
            })
            .await
            .unwrap();
        let mut seqno = 1;
        let out = drive(&parent, &mut output, &mut seqno, |exec| {
            let Some(pb::exec_server_message::Message::SubagentArgs(args)) = &exec.message else {
                panic!("unexpected execution")
            };
            assert_eq!(args.run_in_background, Some(background));
            vec![subagent_result_success(exec.id, "child-conversation")]
        })
        .await;
        assert_eq!(out.terminal, serde_json::json!({}));
        assert_eq!(
            identity(&store, "parent").await,
            ("root".into(), None, None, None)
        );
        let parent_run_id: String =
            sqlx::query_scalar("SELECT run_id FROM runs WHERE cursor_request_id = 'parent'")
                .fetch_one(store.pool())
                .await
                .unwrap();

        // A newer execution of the same Cursor request has no ownership of task-call.
        // A fresh registry also proves that no in-memory parent handle is required.
        provider.push(text_response("new-parent-answer", "another run"));
        let newer_registry = registry(store.clone(), provider.clone());
        let newer = newer_registry.get_or_create("parent").await.unwrap();
        let mut output = newer.subscribe().unwrap();
        newer
            .command(TransportCommand::Append {
                seqno: 0,
                message: Box::new(run_request(
                    "parent-conversation",
                    "parent",
                    "test-model",
                    None,
                    user_message_action(
                        "next",
                        "next-parent-user",
                        Some(pb::RequestContext::default()),
                    ),
                )),
            })
            .await
            .unwrap();
        let mut seqno = 1;
        assert_eq!(
            drive(&newer, &mut output, &mut seqno, |_| vec![])
                .await
                .terminal,
            serde_json::json!({})
        );
        let count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM runs WHERE cursor_request_id = 'parent'")
                .fetch_one(store.pool())
                .await
                .unwrap();
        assert_eq!(count, 2);

        for (request_id, subagent_type) in [("child", Some("explore")), ("child-resume", None)] {
            provider.push(text_response(request_id, "child done"));
            let child_registry = registry(store.clone(), provider.clone());
            let child = child_registry.get_or_create(request_id).await.unwrap();
            child
                .set_parent(TransportParent {
                    request_id: "parent".into(),
                    tool_call_id: "task-call".into(),
                })
                .unwrap();
            let mut request = run_request(
                "child-conversation",
                request_id,
                "test-model",
                None,
                user_message_action("work", request_id, Some(pb::RequestContext::default())),
            );
            let Some(pb::agent_client_message::Message::RunRequest(run)) = request.message.as_mut()
            else {
                unreachable!()
            };
            run.subagent_type_name = subagent_type.map(str::to_owned);
            let mut output = child.subscribe().unwrap();
            child
                .command(TransportCommand::Append {
                    seqno: 0,
                    message: Box::new(request),
                })
                .await
                .unwrap();
            let mut seqno = 1;
            assert_eq!(
                drive(&child, &mut output, &mut seqno, |_| vec![])
                    .await
                    .terminal,
                serde_json::json!({})
            );
            assert_eq!(
                identity(&store, request_id).await,
                (
                    "subagent".into(),
                    Some(parent_run_id.clone()),
                    Some("task-call".into()),
                    Some("explore".into()),
                )
            );
        }
    }
}
