//! Official child failures use the original parent's Task execution, not a new run.
mod support;

use cursor_server::{
    cursor::{
        protocol::proto::agent::v1 as pb, services::official_error, TransportCommand,
        TransportParent, TransportRoute,
    },
    model::ProjectedContent,
};
use support::*;

#[tokio::test]
async fn official_model_failure_completes_parent_task_once_with_original_identity() {
    let (_directory, store) = temp_store().await;
    let provider = FakeProvider::default();
    provider.push(tool_response("parent-call", "task-call", "Task", &serde_json::json!({"prompt":"work", "description":"Child", "model":"nonexistent-official-model"}).to_string()));
    provider.push(text_response("parent-retry", "choose an available ID"));
    let registry = registry(store, provider.clone());
    let parent = registry.get_or_create("parent").await.unwrap();
    let mut output = parent.subscribe().unwrap();
    parent
        .command(TransportCommand::Append {
            seqno: 0,
            message: Box::new(run_request(
                "parent-conversation",
                "parent",
                "test-model",
                None,
                user_message_action("delegate", "user", Some(pb::RequestContext::default())),
            )),
        })
        .await
        .unwrap();
    let mut seqno = 1;
    let out = drive(&parent, &mut output, &mut seqno, |exec| {
        let Some(pb::exec_server_message::Message::SubagentArgs(args)) = &exec.message else {
            panic!("unexpected exec")
        };
        assert_eq!(args.model_id, "nonexistent-official-model");
        let registry = registry.clone();
        let call_id = args.tool_call_id.clone();
        tokio::spawn(async move {
            registry.mark_upstream("official-child").await;
            registry
                .associate_upstream_task(
                    "official-child",
                    TransportParent {
                        request_id: "parent".into(),
                        tool_call_id: call_id,
                    },
                    "nonexistent-official-model".into(),
                )
                .await;
            let TransportRoute::Upstream(generation) = registry.wait_route("official-child").await
            else {
                panic!()
            };
            let payload = support::official_model_not_found_payload();
            let mut frame = vec![2];
            frame.extend_from_slice(&(payload.len() as u32).to_be_bytes());
            frame.extend_from_slice(&payload);
            let chunks: Vec<_> = frame
                .chunks(3)
                .map(|chunk| {
                    Ok::<_, std::convert::Infallible>(axum::body::Bytes::copy_from_slice(chunk))
                })
                .collect();
            let response = axum::http::Response::new(axum::body::Body::from_stream(
                futures_util::stream::iter(chunks),
            ));
            let response = cursor_server::api::cursor::run_sse::upstream(
                registry.clone(),
                "official-child".into(),
                generation,
                response,
                None,
            )
            .await;
            let forwarded = axum::body::to_bytes(response.into_body(), 1024 * 1024)
                .await
                .unwrap();
            assert_eq!(forwarded.as_ref(), frame);
            let error = official_error::extract_end_stream(&payload).unwrap();
            registry
                .fail_upstream_task("official-child", generation, &error)
                .await;
        });
        vec![]
    })
    .await;
    assert_eq!(out.terminal, serde_json::json!({}));
    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    let results: Vec<_> = requests[1]
        .history
        .iter()
        .filter_map(|message| match &message.content {
            ProjectedContent::ToolResult(result) => Some(result),
            _ => None,
        })
        .collect();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].call_id, "task-call");
    assert!(results[0].is_error);
    assert!(results[0].content.contains("available model ID"));
    assert!(results[0]
        .content
        .contains("Model ID: nonexistent-official-model"));
    assert!(results[0].content.contains("Model parameters: {}"));
}
