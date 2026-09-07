//! Verifies explicit and automatic context compaction behavior.
mod support;

use std::collections::HashMap;

use support::{
    drive, openai_model_input, registry, run_request, temp_store, text_response_with_usage,
    user_message_action, FakeProvider,
};

use cursor_server::{
    cursor::{protocol::proto::agent::v1 as pb, TransportCommand, TransportRegistry},
    model::{ContentPart, ConversationId, MessageContent, Origin, ProjectedContent, Role, Usage},
    provider::{FinishReason, ModelEvent},
};
use prost::Message;

#[tokio::test]
async fn summarize_replaces_model_history_and_preserves_cursor_history() {
    let (_directory, store) = temp_store().await;
    let model = store
        .create_model(&openai_model_input("test-model", None))
        .await
        .unwrap();
    let provider = FakeProvider::default();
    provider.push(text_response_with_usage(
        "call-old answer",
        "old answer",
        4_000,
        12,
    ));
    provider.push(vec![
        ModelEvent::Start {
            model_call_id: "summary-call".into(),
        },
        ModelEvent::TextStart,
        ModelEvent::TextDelta("Durable ".into()),
        ModelEvent::TextDelta("summary".into()),
        ModelEvent::TextEnd,
        ModelEvent::Usage(Usage {
            input_tokens: Some(4_012),
            context_input_tokens: Some(4_012),
            output_tokens: Some(9),
            total_tokens: Some(4_021),
            ..Default::default()
        }),
        ModelEvent::Done(FinishReason::Stop),
    ]);
    provider.push(text_response_with_usage(
        "call-new answer",
        "new answer",
        900,
        5,
    ));
    let registry = registry(store.clone(), provider.clone());

    let first = run(
        &registry,
        "first",
        user_request(
            "conversation",
            "user-1",
            "remember alpha",
            &model.model_hash,
            None,
        ),
    )
    .await;
    let first_state = first.checkpoints.last().unwrap().clone();
    let old_turns = first_state.turns.clone();
    let old_roots = first_state.root_prompt_messages_json.clone();
    assert!(old_roots.len() >= 3);

    let compacted = run(
        &registry,
        "compact",
        summary_request("conversation", &model.model_hash, first_state),
    )
    .await;
    assert_eq!(compacted.summary_started, 1);
    assert_eq!(compacted.summary, "Durable summary");
    assert_eq!(compacted.summary_completed, 1);
    assert_eq!(compacted.turn_ended, 1);
    assert_eq!(compacted.token_delta, 0);
    assert_eq!(compacted.checkpoints.len(), 3);
    assert!(compacted
        .checkpoints
        .windows(2)
        .all(|pair| pair[0] == pair[1]));

    let compacted_state = compacted.checkpoints.last().unwrap();
    assert_eq!(compacted_state.root_prompt_messages_json.len(), 2);
    assert!(compacted_state.turns.starts_with(&old_turns));
    assert_eq!(compacted_state.turns.len(), old_turns.len() + 1);
    assert_eq!(compacted_state.self_summary_count, 1);
    let summary_id = compacted_state.summary.as_ref().unwrap();
    let summary = pb::ConversationSummary::decode(compacted.blobs[summary_id].as_slice()).unwrap();
    assert_eq!(summary.summary, "Durable summary");
    let archive_id = compacted_state.summary_archive.as_ref().unwrap();
    let archive =
        pb::ConversationSummaryArchive::decode(compacted.blobs[archive_id].as_slice()).unwrap();
    assert_eq!(archive.summary, "Durable summary");
    assert_eq!(archive.window_tail, 0);
    assert_eq!(archive.summarized_messages, old_roots[1..]);
    assert_eq!(
        archive.summary_message,
        *compacted_state.root_prompt_messages_json.last().unwrap()
    );

    let stored = store
        .load_current_messages(&ConversationId::new("conversation"))
        .await
        .unwrap();
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].origin, Origin::Runtime);
    assert_eq!(stored[0].role, Role::User);
    assert!(matches!(
        &stored[0].content,
        MessageContent::Parts { parts }
            if matches!(parts.as_slice(), [ContentPart::Text { text }]
                if text == "<conversation_summary>\nDurable summary\n</conversation_summary>")
    ));

    let after = run(
        &registry,
        "after",
        user_request(
            "conversation",
            "user-2",
            "what remains?",
            &model.model_hash,
            Some(compacted_state.clone()),
        ),
    )
    .await;
    assert!(after
        .checkpoints
        .last()
        .unwrap()
        .root_prompt_messages_json
        .starts_with(&compacted_state.root_prompt_messages_json));
    let requests = provider.requests();
    assert_eq!(requests.len(), 3);
    assert!(requests[1].prompt.tools.is_empty());
    assert!(requests[1]
        .prompt
        .instructions
        .contains("compacting conversation history"));
    assert_eq!(requests[1].history.len(), 3);
    assert_eq!(
        requests[1].history[2].message_id, "compaction:instruction",
        "an assistant-terminated history gets the summarize instruction as its user tail"
    );
    assert_eq!(requests[2].history.len(), 2);
    let ProjectedContent::Parts(summary_parts) = &requests[2].history[0].content else {
        panic!("first post-compaction message must be the summary")
    };
    assert!(
        matches!(summary_parts.as_slice(), [ContentPart::Text { text }]
        if text.contains("Durable summary"))
    );
    let ProjectedContent::Parts(new_user_parts) = &requests[2].history[1].content else {
        panic!("second post-compaction message must be the new runtime user")
    };
    assert!(
        matches!(new_user_parts.as_slice(), [ContentPart::Text { text }]
        if text.contains("what remains?") && !text.contains("remember alpha"))
    );
}

#[tokio::test]
async fn automatic_compaction_preflights_provider_input_and_records_rebuilt_tokens() {
    let (_directory, store) = temp_store().await;
    let model = store
        .create_model(&openai_model_input("auto-compact-model", Some(100_000)))
        .await
        .unwrap();
    let provider = FakeProvider::default();
    let long_answer = "x".repeat(400_000);
    provider.push(text_response_with_usage(
        &format!("call-{long_answer}"),
        &long_answer,
        150_000,
        1_000,
    ));
    provider.push(text_response_with_usage(
        "call-automatic durable summary",
        "automatic durable summary",
        120_000,
        20,
    ));
    provider.push(text_response_with_usage(
        "call-continued after compaction",
        "continued after compaction",
        20_000,
        20,
    ));
    let registry = registry(store, provider.clone());

    let first = run(
        &registry,
        "auto-first",
        user_request(
            "auto-conversation",
            "auto-user-1",
            "start",
            &model.model_hash,
            None,
        ),
    )
    .await;
    let first_state = first.checkpoints.last().unwrap().clone();
    assert!(first_state.token_details.as_ref().unwrap().used_tokens > 100_000);

    let second = run(
        &registry,
        "auto-second",
        user_request(
            "auto-conversation",
            "auto-user-2",
            "continue",
            &model.model_hash,
            Some(first_state),
        ),
    )
    .await;
    assert_eq!(second.summary_started, 1);
    assert_eq!(second.summary_completed, 1);
    assert_eq!(
        &second.interaction_events[..4],
        &[
            "token_delta:0",
            "summary_started",
            "summary_completed",
            "token_delta:0",
        ],
        "automatic compaction must publish estimated usage before summarizing and zero usage after"
    );
    let compacted_tokens = second
        .checkpoints
        .iter()
        .filter_map(|state| state.token_details.as_ref())
        .map(|details| details.used_tokens)
        .find(|tokens| *tokens > 0 && *tokens < 100_000)
        .expect("compacted checkpoint must record rebuilt context tokens");
    assert!(compacted_tokens < 90_000);

    let requests = provider.requests();
    assert_eq!(requests.len(), 3);
    assert!(!requests[0].prompt.tools.is_empty());
    assert!(requests[1].prompt.tools.is_empty());
    assert!(!requests[2].prompt.tools.is_empty());
    assert!(requests[1]
        .history
        .iter()
        .any(|message| match &message.content {
            ProjectedContent::Assistant { text, .. } => text.len() == 400_000,
            _ => false,
        }));
    assert!(requests[2]
        .history
        .iter()
        .all(|message| match &message.content {
            ProjectedContent::Assistant { text, .. } => text.len() != 400_000,
            _ => true,
        }));
}

#[tokio::test]
async fn incremental_preflight_uses_conversation_anchor_across_model_switch() {
    let (_directory, store) = temp_store().await;
    let model_a = store
        .create_model(&openai_model_input("anchor-model-a", None))
        .await
        .unwrap();
    let model_b = store
        .create_model(&openai_model_input("anchor-model-b", Some(200_000)))
        .await
        .unwrap();
    let provider = FakeProvider::default();
    provider.push(text_response_with_usage(
        "call-old answer",
        "old answer",
        103_904,
        12,
    ));
    provider.push(text_response_with_usage(
        "call-new answer",
        "new answer",
        104_000,
        12,
    ));
    let registry = registry(store, provider.clone());

    let first = run(
        &registry,
        "anchor-first",
        user_request(
            "anchor-conversation",
            "anchor-user-1",
            &"x".repeat(400_000),
            &model_a.model_hash,
            None,
        ),
    )
    .await;
    let second = run(
        &registry,
        "anchor-second",
        user_request(
            "anchor-conversation",
            "anchor-user-2",
            "short follow-up",
            &model_b.model_hash,
            first.checkpoints.last().cloned(),
        ),
    )
    .await;

    assert_eq!(second.summary_started, 0);
    assert_eq!(second.summary_completed, 0);
    assert_eq!(provider.requests().len(), 2);
}

#[tokio::test]
async fn irreducibly_oversized_current_input_fails_before_provider_dispatch() {
    let (_directory, store) = temp_store().await;
    let model = store
        .create_model(&openai_model_input("overflow-model", Some(100_000)))
        .await
        .unwrap();
    let provider = FakeProvider::default();
    let registry = registry(store, provider.clone());

    let output = run(
        &registry,
        "overflow-request",
        user_request(
            "overflow-conversation",
            "overflow-user",
            &"x".repeat(400_000),
            &model.model_hash,
            None,
        ),
    )
    .await;

    assert!(provider.requests().is_empty());
    assert_eq!(output.summary_started, 0);
    assert_eq!(output.summary_completed, 0);
}

#[tokio::test]
async fn provider_overflow_refusal_compacts_and_retries_once() {
    // The estimate cleared the compaction check, but the provider counted
    // more and refused. The refusal is the trigger the estimate missed.
    let (_directory, store) = temp_store().await;
    let model = store
        .create_model(&openai_model_input("refusal-model", Some(1_000_000)))
        .await
        .unwrap();
    let provider = FakeProvider::default();
    provider.push(text_response_with_usage(
        "call-first answer",
        "first answer",
        4_000,
        12,
    ));
    provider.push_error(cursor_server::Error::Provider(
        "Anthropic 400 Bad Request: {\"type\":\"error\",\"error\":{\"type\":\
         \"invalid_request_error\",\"message\":\"prompt is too long: 1002148 tokens > \
         1000000 maximum\"}}"
            .into(),
    ));
    provider.push(text_response_with_usage(
        "call-durable summary",
        "durable summary",
        3_000,
        20,
    ));
    provider.push(text_response_with_usage(
        "call-answer after compaction",
        "answer after compaction",
        500,
        20,
    ));
    let registry = registry(store, provider.clone());

    let first = run(
        &registry,
        "refusal-first",
        user_request(
            "refusal-conversation",
            "refusal-user-1",
            "start",
            &model.model_hash,
            None,
        ),
    )
    .await;
    let second = run(
        &registry,
        "refusal-second",
        user_request(
            "refusal-conversation",
            "refusal-user-2",
            "continue",
            &model.model_hash,
            first.checkpoints.last().cloned(),
        ),
    )
    .await;

    assert_eq!(second.summary_started, 1);
    assert_eq!(second.summary_completed, 1);
    assert_eq!(second.turn_ended, 1);
    let requests = provider.requests();
    assert_eq!(
        requests.len(),
        4,
        "refused call, summary call, retried call"
    );
    assert!(requests[2].prompt.tools.is_empty());
    assert_eq!(
        requests[2].history.last().unwrap().role,
        Role::User,
        "the summarizer history must end with a user message"
    );
    assert!(!requests[3].prompt.tools.is_empty());
    assert!(requests[3]
        .history
        .iter()
        .any(|message| match &message.content {
            ProjectedContent::Parts(parts) =>
                matches!(parts.as_slice(), [ContentPart::Text { text }]
            if text.contains("durable summary")),
            _ => false,
        }));
}

#[tokio::test]
async fn resumed_history_stays_user_terminated_without_persisting_phantom_tails() {
    // A resume never replays an assistant-terminated history at the provider:
    // the local resume flow appends a `resume-prompt` user message of its own
    // (cursor/compile/break_messages.rs), so the user_terminated projection
    // must not stack a second, phantom tail on top. Whatever tail the run
    // projects is provider-visible only and must never reach the store.
    let (_directory, store) = temp_store().await;
    let model = store
        .create_model(&openai_model_input("tail-model", None))
        .await
        .unwrap();
    let provider = FakeProvider::default();
    provider.push(text_response_with_usage(
        "call-first answer",
        "first answer",
        400,
        12,
    ));
    provider.push(text_response_with_usage(
        "call-resumed answer",
        "resumed answer",
        450,
        12,
    ));
    let registry = registry(store.clone(), provider.clone());

    let first = run(
        &registry,
        "tail-first",
        user_request(
            "tail-conversation",
            "tail-user-1",
            "start",
            &model.model_hash,
            None,
        ),
    )
    .await;
    let resumed = run(
        &registry,
        "tail-resume",
        run_request(
            "tail-conversation",
            "reusable-wire-run-id",
            &model.model_hash,
            first.checkpoints.last().cloned(),
            pb::conversation_action::Action::ResumeAction(pb::ResumeAction::default()),
        ),
    )
    .await;
    assert_eq!(resumed.turn_ended, 1);

    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    let tail = requests[1].history.last().unwrap();
    assert_eq!(tail.role, Role::User);
    assert_eq!(
        tail.message_id, "resume-prompt:tail-resume",
        "the resume prompt is the user tail the provider sees"
    );
    assert!(
        requests[1]
            .history
            .iter()
            .all(|message| message.message_id != "runtime:continue"),
        "an already user-terminated history must not gain a transient continuation tail"
    );
    assert_eq!(
        requests[1].history.len(),
        requests[0].history.len() + 2,
        "committed history plus the first answer, then the resume prompt"
    );
    let stored = store
        .load_current_messages(&ConversationId::new("tail-conversation"))
        .await
        .unwrap();
    assert!(
        stored
            .iter()
            .all(|message| message.message_id != "runtime:continue"),
        "projected tails are provider-visible only and never persisted"
    );
}

#[derive(Default)]
struct Output {
    checkpoints: Vec<pb::ConversationStateStructure>,
    blobs: HashMap<Vec<u8>, Vec<u8>>,
    summary: String,
    summary_started: usize,
    summary_completed: usize,
    turn_ended: usize,
    token_delta: usize,
    interaction_events: Vec<String>,
}

async fn run(
    registry: &TransportRegistry,
    request_id: &str,
    request: pb::AgentClientMessage,
) -> Output {
    let handle = registry.get_or_create(request_id).await.unwrap();
    let mut receiver = handle.subscribe();
    handle
        .command(TransportCommand::Append {
            seqno: 0,
            message: Box::new(request),
        })
        .await
        .unwrap();
    let mut append_seqno = 1;
    let streamed = drive(&handle, &mut receiver, &mut append_seqno, |_| vec![]).await;
    let mut output = Output {
        checkpoints: streamed.checkpoints,
        blobs: streamed.blobs,
        ..Default::default()
    };
    for update in streamed.interactions {
        match update.message {
            Some(pb::interaction_update::Message::SummaryStarted(_)) => {
                output.summary_started += 1;
                output.interaction_events.push("summary_started".into());
            }
            Some(pb::interaction_update::Message::Summary(delta)) => {
                output.summary.push_str(&delta.summary)
            }
            Some(pb::interaction_update::Message::SummaryCompleted(_)) => {
                output.summary_completed += 1;
                output.interaction_events.push("summary_completed".into());
            }
            Some(pb::interaction_update::Message::TurnEnded(_)) => output.turn_ended += 1,
            Some(pb::interaction_update::Message::TokenDelta(delta)) => {
                output.token_delta += 1;
                output
                    .interaction_events
                    .push(format!("token_delta:{}", delta.tokens));
            }
            _ => {}
        }
    }
    output
}

fn user_request(
    conversation_id: &str,
    message_id: &str,
    text: &str,
    model_id: &str,
    state: Option<pb::ConversationStateStructure>,
) -> pb::AgentClientMessage {
    run_request(
        conversation_id,
        "reusable-wire-run-id",
        model_id,
        state,
        user_message_action(text, message_id, Some(pb::RequestContext::default())),
    )
}

fn summary_request(
    conversation_id: &str,
    model_id: &str,
    state: pb::ConversationStateStructure,
) -> pb::AgentClientMessage {
    run_request(
        conversation_id,
        "reusable-wire-run-id",
        model_id,
        Some(state),
        user_message_action(
            "/summarize",
            "summary-command",
            Some(pb::RequestContext::default()),
        ),
    )
}
