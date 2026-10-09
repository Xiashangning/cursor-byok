//! Renders captured protocol messages as protobuf JSON for detailed traces.
//!
//! Detailed logging must stay readable without recompiling the server, so the
//! build embeds a descriptor set and every captured message is rendered with
//! proto JSON naming: `bytes` become base64 and enums become names. The paired
//! raw trace artifact remains the source of truth for exact wire bytes.

use std::sync::OnceLock;

use prost::Message;
use prost_reflect::{DescriptorPool, DynamicMessage, SerializeOptions};

use super::proto::agent::v1 as pb;
use crate::model::utf8_prefix;

/// Upper bound for one rendered message; larger payloads are cut and flagged.
pub const MAX_RENDERED_BYTES: usize = 256 * 1024;

const CLIENT_MESSAGE: &str = "agent.v1.AgentClientMessage";
const SERVER_MESSAGE: &str = "agent.v1.AgentServerMessage";
const CONVERSATION_STATE: &str = "agent.v1.ConversationStateStructure";

/// One rendered message, cut to `MAX_RENDERED_BYTES` when it is too large.
pub struct Rendered {
    pub json: Vec<u8>,
    pub truncated: bool,
    pub total_bytes: usize,
}

pub fn render_client(message: &pb::AgentClientMessage) -> Option<Rendered> {
    render(CLIENT_MESSAGE, message)
}

pub fn render_server(message: &pb::AgentServerMessage) -> Option<Rendered> {
    render(SERVER_MESSAGE, message)
}

pub fn render_state(state: &pb::ConversationStateStructure) -> Option<Rendered> {
    render(CONVERSATION_STATE, state)
}

fn render<M: Message>(name: &str, message: &M) -> Option<Rendered> {
    let descriptor = pool().get_message_by_name(name)?;
    let decoded = DynamicMessage::decode(descriptor, message.encode_to_vec().as_slice()).ok()?;
    let mut serializer = serde_json::Serializer::new(Vec::new());
    decoded
        .serialize_with_options(&mut serializer, &SerializeOptions::new())
        .ok()?;
    let mut json = serializer.into_inner();
    let total_bytes = json.len();
    let truncated = total_bytes > MAX_RENDERED_BYTES;
    if truncated {
        // Truncate on a UTF-8 character boundary; prost JSON output is valid UTF-8.
        let end = utf8_prefix(std::str::from_utf8(&json).ok()?, MAX_RENDERED_BYTES).len();
        json.truncate(end);
    }
    Some(Rendered {
        json,
        truncated,
        total_bytes,
    })
}

fn pool() -> &'static DescriptorPool {
    static POOL: OnceLock<DescriptorPool> = OnceLock::new();
    POOL.get_or_init(|| {
        DescriptorPool::decode(
            include_bytes!(concat!(env!("OUT_DIR"), "/cursor_protocol_descriptors.bin")).as_slice(),
        )
        .expect("embedded Cursor protocol descriptors")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_messages_render_as_structured_json() {
        let message = pb::AgentClientMessage {
            message: Some(pb::agent_client_message::Message::RunRequest(
                pb::AgentRunRequest {
                    conversation_id: Some("conversation-1".into()),
                    action: Some(pb::ConversationAction {
                        action: Some(pb::conversation_action::Action::UserMessageAction(
                            pb::UserMessageAction {
                                user_message: Some(pb::UserMessage {
                                    text: "hello".into(),
                                    message_id: "user-1".into(),
                                    ..Default::default()
                                }),
                                ..Default::default()
                            },
                        )),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
            )),
        };
        let rendered = render_client(&message).expect("client message renders");
        let value: serde_json::Value = serde_json::from_slice(&rendered.json).unwrap();
        assert_eq!(value["runRequest"]["conversationId"], "conversation-1");
        assert_eq!(
            value["runRequest"]["action"]["userMessageAction"]["userMessage"]["text"],
            "hello"
        );
        assert!(!rendered.truncated);
        assert_eq!(rendered.total_bytes, rendered.json.len());
    }

    #[test]
    fn server_messages_render_blob_ids_as_base64() {
        let message = pb::AgentServerMessage {
            ttft_breakdown: None,
            message: Some(
                pb::agent_server_message::Message::ConversationCheckpointUpdate(
                    pb::ConversationStateStructure {
                        root_prompt_messages_json: vec![vec![1, 2, 3]],
                        ..Default::default()
                    },
                ),
            ),
        };
        let rendered = render_server(&message).expect("server message renders");
        let value: serde_json::Value = serde_json::from_slice(&rendered.json).unwrap();
        assert_eq!(
            value["conversationCheckpointUpdate"]["rootPromptMessagesJson"][0],
            "AQID"
        );
    }

    #[test]
    fn oversized_messages_are_cut_at_a_character_boundary() {
        let message = pb::AgentClientMessage {
            message: Some(pb::agent_client_message::Message::RunRequest(
                pb::AgentRunRequest {
                    action: Some(pb::ConversationAction {
                        action: Some(pb::conversation_action::Action::UserMessageAction(
                            pb::UserMessageAction {
                                user_message: Some(pb::UserMessage {
                                    text: "中".repeat(MAX_RENDERED_BYTES),
                                    ..Default::default()
                                }),
                                ..Default::default()
                            },
                        )),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
            )),
        };
        let rendered = render_client(&message).expect("client message renders");
        assert!(rendered.truncated);
        assert!(rendered.json.len() <= MAX_RENDERED_BYTES);
        assert!(rendered.total_bytes > MAX_RENDERED_BYTES);
        assert!(std::str::from_utf8(&rendered.json).is_ok());
    }
}
