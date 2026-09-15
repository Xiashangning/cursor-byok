//! Exercises Cursor model selection through transport compilation and provider requests.
mod support;

use cursor_server::{
    cursor::{protocol::proto::agent::v1 as pb, TransportCommand},
    model::ModelLatency,
};
use support::{
    drive, openai_model_input, registry, run_request, temp_store, text_response,
    user_message_action, FakeProvider,
};

#[tokio::test]
async fn resumed_child_model_selection_survives_parent_checkpoint_roundtrip() {
    child_model_checkpoint_roundtrip(true).await;
}

#[tokio::test]
async fn foreground_child_model_selection_survives_parent_checkpoint_roundtrip() {
    child_model_checkpoint_roundtrip(false).await;
}

async fn child_model_checkpoint_roundtrip(background: bool) {
    let (_directory, store) = temp_store().await;
    let mut config = openai_model_input("provider-model", Some(200_000));
    config.context_options = vec!["200k".into(), "1m".into()];
    config.effort_options = vec!["low".into(), "high".into()];
    config.reasoning_effort = Some("high".into());
    let model = store.create_model(&config).await.unwrap();
    let provider = FakeProvider::default();
    provider.push(support::tool_response("create", "create-call", "Task", &serde_json::json!({
        "description":"create", "prompt":"first", "model":model.model_hash,
        "run_in_background":background, "model_parameters":[{"id":"context","value":"1m"},{"id":"reasoning","value":"low"}]
    }).to_string()));
    provider.push(support::tool_response("resume", "resume-call", "Task", &serde_json::json!({
        "description":"resume", "prompt":"second", "resume":"child", "model":model.model_hash,
        "run_in_background":background, "model_parameters":[{"id":"context","value":"1m"},{"id":"reasoning","value":"high"}]
    }).to_string()));
    provider.push(text_response("done-first", "done"));
    provider.push(support::tool_response(
        "update",
        "update-call",
        "send-message-to-agent",
        r#"{"agent_id":"child","prompt":"third"}"#,
    ));
    provider.push(text_response("done-second", "done"));
    let registry = registry(store, provider.clone());
    let mut checkpoint = None;
    for turn in 0..2 {
        let handle = registry
            .get_or_create(&format!("parent-{turn}"))
            .await
            .unwrap();
        let mut output = handle.subscribe();
        handle
            .command(TransportCommand::Append {
                seqno: 0,
                message: Box::new(run_request(
                    "parent",
                    &format!("run-{turn}"),
                    &model.model_hash,
                    checkpoint.take(),
                    user_message_action(
                        "delegate",
                        &format!("user-{turn}"),
                        Some(pb::RequestContext::default()),
                    ),
                )),
            })
            .await
            .unwrap();
        let mut seqno = 1;
        let mut calls = 0;
        let out = drive(&handle, &mut output, &mut seqno, |exec| {
            let Some(pb::exec_server_message::Message::SubagentArgs(args)) = &exec.message else {
                panic!("expected child execution")
            };
            let expected = if turn == 0 && calls == 0 {
                "1m-low"
            } else {
                "1m-high"
            };
            assert_eq!(args.model_id, format!("{}-{expected}", model.model_hash));
            if turn == 1 {
                assert_eq!(args.resume_agent_id.as_deref(), Some("child"));
            }
            calls += 1;
            let mut reply = support::subagent_result_success(exec.id, "child");
            if let Some(pb::agent_client_message::Message::ExecClientMessage(exec)) =
                reply.message.as_mut()
            {
                if let Some(pb::exec_client_message::Message::SubagentResult(result)) =
                    exec.message.as_mut()
                {
                    if let Some(pb::subagent_result::Result::Success(success)) =
                        result.result.as_mut()
                    {
                        if background {
                            success.background_reason =
                                pb::SubagentBackgroundReason::AgentRequest as i32;
                        }
                    }
                }
            }
            vec![reply, support::stream_close(exec.id)]
        })
        .await;
        assert_eq!(out.terminal, serde_json::json!({}));
        checkpoint = out.checkpoints.last().cloned();
    }
    assert_eq!(provider.request_count(), 5);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn subagent_variant_slug_wins_over_echoed_parameters_for_concurrent_child_runs() {
    let (_directory, store) = temp_store().await;
    let mut config = openai_model_input("provider-model", Some(200_000));
    config.context_options = vec!["200k".into(), "1m".into()];
    config.effort_options = vec!["low".into(), "high".into()];
    config.reasoning_effort = Some("high".into());
    let model = store.create_model(&config).await.unwrap();
    let provider = FakeProvider::default();
    for _ in 0..8 {
        provider.push(text_response("answer", "done"));
    }
    let registry = registry(store, provider.clone());
    let jobs = (0..8).map(|index| {
        let registry = registry.clone();
        let model_id = format!("{}-200k-high-fast", model.model_hash);
        async move {
            let id = format!("child-{index}");
            let handle = registry.get_or_create(&id).await.unwrap();
            let mut request = run_request(
                &id,
                &id,
                &model_id,
                None,
                user_message_action(
                    &format!("work for {id}"),
                    &id,
                    Some(pb::RequestContext::default()),
                ),
            );
            let Some(pb::agent_client_message::Message::RunRequest(run)) = request.message.as_mut()
            else {
                unreachable!()
            };
            run.subagent_type_name = Some("generalPurpose".into());
            // Cursor 回程可能回声与 slug 冲突的客户端参数;子代理的 slug 是
            // 父代理 Task 调用烘焙的权威选择,必须压过这些回声。
            run.requested_model.as_mut().unwrap().parameters =
                [("context", "1m"), ("reasoning", "low"), ("fast", "false")]
                    .into_iter()
                    .map(|(id, value)| pb::requested_model::ModelParameterValue {
                        id: id.into(),
                        value: value.into(),
                    })
                    .collect();
            let mut output = handle.subscribe();
            handle
                .command(TransportCommand::Append {
                    seqno: 0,
                    message: Box::new(request),
                })
                .await
                .unwrap();
            let mut seqno = 1;
            let out = drive(&handle, &mut output, &mut seqno, |_| {
                panic!("context supplied inline")
            })
            .await;
            assert_eq!(out.terminal, serde_json::json!({}));
        }
    });
    futures_util::future::join_all(jobs).await;
    let requests = provider.requests();
    assert_eq!(requests.len(), 8);
    let mut seen = std::collections::HashSet::new();
    for request in requests {
        let history = serde_json::to_string(&request.history).unwrap();
        let owners: Vec<_> = (0..8)
            .filter(|index| history.contains(&format!("work for child-{index}")))
            .collect();
        assert_eq!(
            owners.len(),
            1,
            "provider history must not mix concurrent conversations"
        );
        assert!(seen.insert(owners[0]));
        assert_eq!(request.model.context_window_tokens, Some(200_000));
        assert_eq!(request.model.reasoning.effort.as_deref(), Some("high"));
        assert_eq!(request.model.latency, ModelLatency::Fast);
    }
}

#[tokio::test]
async fn root_run_parameters_override_the_selected_variant() {
    let (_directory, store) = temp_store().await;
    let mut config = openai_model_input("provider-model", Some(200_000));
    config.context_options = vec!["200k".into(), "1m".into()];
    config.effort_options = vec!["low".into(), "high".into()];
    config.reasoning_effort = Some("high".into());
    let model = store.create_model(&config).await.unwrap();
    let provider = FakeProvider::default();
    provider.push(text_response("answer", "done"));
    let registry = registry(store, provider.clone());
    let handle = registry.get_or_create("root-run").await.unwrap();
    let mut request = run_request(
        "root-run",
        "root-run",
        &format!("{}-200k-high-fast", model.model_hash),
        None,
        user_message_action("work", "root-user", Some(pb::RequestContext::default())),
    );
    let Some(pb::agent_client_message::Message::RunRequest(run)) = request.message.as_mut() else {
        unreachable!()
    };
    // 根会话的参数来自模型选择器换档,仍然是显式用户意图,覆盖 slug。
    run.requested_model.as_mut().unwrap().parameters =
        [("context", "1m"), ("reasoning", "low"), ("fast", "false")]
            .into_iter()
            .map(|(id, value)| pb::requested_model::ModelParameterValue {
                id: id.into(),
                value: value.into(),
            })
            .collect();
    let mut output = handle.subscribe();
    handle
        .command(TransportCommand::Append {
            seqno: 0,
            message: Box::new(request),
        })
        .await
        .unwrap();
    let mut seqno = 1;
    let out = drive(&handle, &mut output, &mut seqno, |_| {
        panic!("context supplied inline")
    })
    .await;
    assert_eq!(out.terminal, serde_json::json!({}));
    let requests = provider.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].model.context_window_tokens, Some(1_000_000));
    assert_eq!(requests[0].model.reasoning.effort.as_deref(), Some("low"));
    assert_eq!(requests[0].model.latency, ModelLatency::Standard);
}

#[tokio::test]
async fn subagent_parameters_apply_when_the_model_id_is_not_a_variant_slug() {
    let (_directory, store) = temp_store().await;
    let mut config = openai_model_input("provider-model", Some(200_000));
    config.context_options = vec!["200k".into(), "1m".into()];
    config.effort_options = vec!["low".into(), "high".into()];
    config.reasoning_effort = Some("high".into());
    let model = store.create_model(&config).await.unwrap();
    let provider = FakeProvider::default();
    provider.push(text_response("answer", "done"));
    let registry = registry(store, provider.clone());
    let handle = registry.get_or_create("child-run").await.unwrap();
    let mut request = run_request(
        "child-run",
        "child-run",
        &model.model_hash,
        None,
        user_message_action("work", "child-user", Some(pb::RequestContext::default())),
    );
    let Some(pb::agent_client_message::Message::RunRequest(run)) = request.message.as_mut() else {
        unreachable!()
    };
    run.subagent_type_name = Some("generalPurpose".into());
    run.requested_model.as_mut().unwrap().parameters = [("context", "1m"), ("reasoning", "low")]
        .into_iter()
        .map(|(id, value)| pb::requested_model::ModelParameterValue {
            id: id.into(),
            value: value.into(),
        })
        .collect();
    let mut output = handle.subscribe();
    handle
        .command(TransportCommand::Append {
            seqno: 0,
            message: Box::new(request),
        })
        .await
        .unwrap();
    let mut seqno = 1;
    let out = drive(&handle, &mut output, &mut seqno, |_| {
        panic!("context supplied inline")
    })
    .await;
    assert_eq!(out.terminal, serde_json::json!({}));
    let requests = provider.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].model.context_window_tokens, Some(1_000_000));
    assert_eq!(requests[0].model.reasoning.effort.as_deref(), Some("low"));
}
