//! Verifies local markdown rules are merged into the request-context message.
mod support;

use std::sync::Arc;

use support::{
    kv_ack, prompt_assets, run_request, temp_store, text_response, user_message_action,
    FakeProvider,
};

use cursor_server::{
    cursor::{
        prompting::PromptCompiler, protocol::connect, protocol::proto::agent::v1 as pb,
        TransportCommand, TransportRegistry,
    },
    model::{ContentPart, ProjectedContent},
};
use prost::Message;

#[tokio::test]
async fn local_markdown_rules_land_in_the_request_context_message() {
    let (_store_dir, store) = temp_store().await;
    let provider = FakeProvider::default();
    provider.push(text_response("call-1", "ok"));
    let rules_dir = tempfile::tempdir().unwrap();
    let rules_root = rules_dir.path().join("rules");
    std::fs::create_dir_all(&rules_root).unwrap();
    std::fs::write(rules_root.join("17353272.md"), "Always answer in haiku.").unwrap();

    let registry = TransportRegistry::with_local_rules(
        store,
        Arc::new(provider.clone()),
        PromptCompiler::new(prompt_assets()),
        rules_root,
    );
    let handle = registry.get_or_create("rules-request").await.unwrap();
    let mut output = handle.subscribe();
    handle
        .command(TransportCommand::Append {
            seqno: 0,
            message: Box::new(run_request(
                "rules-conversation",
                "rules-request",
                "test-model",
                None,
                user_message_action("hello", "rules-user", None),
            )),
        })
        .await
        .unwrap();

    let mut append_seqno = 1;
    loop {
        let frame = tokio::time::timeout(std::time::Duration::from_secs(5), output.recv())
            .await
            .expect("run finishes within timeout")
            .expect("output stays open until EndStream");
        let (flags, payload) = connect::decode_frames(&frame).unwrap().pop().unwrap();
        if flags & connect::END_STREAM_FLAG != 0 {
            break;
        }
        // The Run waits for the client to confirm every conversation Blob write,
        // so the stream only advances once each KvServerMessage is acknowledged.
        if let Some(pb::agent_server_message::Message::KvServerMessage(kv)) =
            pb::AgentServerMessage::decode(payload).unwrap().message
        {
            handle
                .command(TransportCommand::Append {
                    seqno: append_seqno,
                    message: Box::new(kv_ack(kv.id)),
                })
                .await
                .unwrap();
            append_seqno += 1;
        }
    }

    let requests = provider.requests();
    assert_eq!(requests.len(), 1);
    let context_texts = requests[0]
        .history
        .iter()
        .filter(|message| message.message_id.starts_with("request-context:"))
        .map(|message| {
            let ProjectedContent::Parts(parts) = &message.content else {
                panic!("request context message must be parts")
            };
            let [ContentPart::Text { text }] = parts.as_slice() else {
                panic!("request context message must be one text part")
            };
            text.clone()
        })
        .collect::<Vec<_>>();
    assert_eq!(
        context_texts.len(),
        1,
        "exactly one request-context message is projected"
    );
    assert!(
        context_texts[0].contains("<user_rule>\nAlways answer in haiku.\n</user_rule>"),
        "local markdown rule must appear as a user rule: {}",
        context_texts[0]
    );

    registry.shutdown().await;
}
