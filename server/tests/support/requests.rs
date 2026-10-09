//! `AgentClientMessage` request fixtures.

#![allow(dead_code)]

use cursor_server::cursor::protocol::proto::agent::v1 as pb;

/// A `RunRequest` carrying `action` against `conversation_id` on `run_id`.
pub fn run_request(
    conversation_id: &str,
    run_id: &str,
    model_id: &str,
    state: Option<pb::ConversationStateStructure>,
    action: pb::conversation_action::Action,
) -> pb::AgentClientMessage {
    pb::AgentClientMessage {
        message: Some(pb::agent_client_message::Message::RunRequest(
            pb::AgentRunRequest {
                action: Some(pb::ConversationAction {
                    action: Some(action),
                    ..Default::default()
                }),
                conversation_id: Some(conversation_id.into()),
                run_id: Some(run_id.into()),
                requested_model: Some(pb::RequestedModel {
                    model_id: model_id.into(),
                    ..Default::default()
                }),
                conversation_state: state,
                ..Default::default()
            },
        )),
    }
}

/// A `UserMessageAction` in Agent mode; `request_context` may be `None`.
pub fn user_message_action(
    text: &str,
    message_id: &str,
    request_context: Option<pb::RequestContext>,
) -> pb::conversation_action::Action {
    pb::conversation_action::Action::UserMessageAction(pb::UserMessageAction {
        user_message: Some(pb::UserMessage {
            text: text.into(),
            message_id: message_id.into(),
            mode: pb::AgentMode::Agent as i32,
            ..Default::default()
        }),
        request_context,
        ..Default::default()
    })
}

/// A `ResumeAction`; `request_context` may be `None` (plain `ResumeAction`).
pub fn resume_action(
    request_context: Option<pb::RequestContext>,
) -> pb::conversation_action::Action {
    pb::conversation_action::Action::ResumeAction(pb::ResumeAction { request_context })
}
