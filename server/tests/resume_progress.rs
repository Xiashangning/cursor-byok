mod support;

use cursor_server::{
    cursor::{protocol::proto::agent::v1 as pb, TransportCommand},
    model::{ContentPart, ProjectedContent, Role},
};
use support::{
    drive, registry, resume_action, run_request, temp_store, text_response, user_message_action,
    FakeProvider,
};

#[tokio::test]
async fn resume_action_context_and_continuation_reach_the_next_model_call() {
    let (_directory, store) = temp_store().await;
    let provider = FakeProvider::default();
    provider.push(text_response("model-initial", "initial response"));
    provider.push(text_response("model-resumed", "continued response"));
    let registry = registry(store, provider.clone());

    let first = registry.get_or_create("initial-request").await.unwrap();
    let mut first_output = first.subscribe().unwrap();
    first
        .command(TransportCommand::Append {
            seqno: 0,
            message: Box::new(run_request(
                "resume-progress-conversation",
                "initial-wire-run",
                "test-model",
                None,
                user_message_action("perform the task", "user-1", Some(request_context("bash"))),
            )),
        })
        .await
        .unwrap();
    let mut first_seqno = 1;
    let mut out = drive(&first, &mut first_output, &mut first_seqno, |_| vec![]).await;
    assert!(out.kvs.iter().all(|kv| matches!(
        kv.message,
        Some(pb::kv_server_message::Message::SetBlobArgs(_))
    )));
    let state = out
        .checkpoints
        .pop()
        .expect("run must publish a checkpoint");
    assert!(state.pending_tool_calls.is_empty());

    let resumed = registry.get_or_create("resume-request").await.unwrap();
    let mut resumed_output = resumed.subscribe().unwrap();
    resumed
        .command(TransportCommand::Append {
            seqno: 0,
            message: Box::new(run_request(
                "resume-progress-conversation",
                "resume-wire-run",
                "test-model",
                Some(state),
                resume_action(Some(request_context("pwsh"))),
            )),
        })
        .await
        .unwrap();
    let mut resumed_seqno = 1;
    let mut out = drive(
        &resumed,
        &mut resumed_output,
        &mut resumed_seqno,
        |_| vec![],
    )
    .await;
    assert!(out.kvs.iter().all(|kv| matches!(
        kv.message,
        Some(pb::kv_server_message::Message::SetBlobArgs(_))
    )));
    let _ = out
        .checkpoints
        .pop()
        .expect("run must publish a checkpoint");

    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    let history = &requests[1].history;
    assert_eq!(
        history
            .iter()
            .filter(|message| message.message_id.starts_with("request-context:"))
            .count(),
        2,
        "the changed ResumeAction context must be appended to history"
    );
    let texts = history
        .iter()
        .filter_map(projected_text)
        .collect::<Vec<_>>();
    assert!(texts.iter().any(|text| text.contains("Shell: pwsh")));
    let last = history.last().expect("resume history must not be empty");
    assert_eq!(last.role, Role::User);
    assert_eq!(last.message_id, "resume-prompt:resume-request");
    assert!(projected_text(last).as_deref().is_some_and(|text| {
        text.contains("<resume>") && text.contains("make concrete progress")
    }));
}

fn request_context(shell: &str) -> pb::RequestContext {
    pb::RequestContext {
        env: Some(pb::RequestContextEnv {
            os_version: "windows".into(),
            workspace_paths: vec!["C:/workspace".into()],
            shell: shell.into(),
            terminals_folder: "C:/terminals".into(),
            agent_transcripts_folder: "C:/transcripts".into(),
            ..Default::default()
        }),
        ..Default::default()
    }
}

fn projected_text(message: &cursor_server::model::ProjectedMessage) -> Option<String> {
    let ProjectedContent::Parts(parts) = &message.content else {
        return None;
    };
    Some(
        parts
            .iter()
            .filter_map(|part| match part {
                ContentPart::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
    )
}
