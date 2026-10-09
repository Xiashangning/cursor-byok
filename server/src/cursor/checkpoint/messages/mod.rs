//! Converts between canonical Messages and Cursor checkpoint message data.

mod decode;
mod encode;

pub use decode::{decode, decode_pending};
pub use encode::{stable_messages, staged_final, staged_tool_round, with_completed_tool_messages};

const COMPLETED_TOOL_MESSAGES_FIELD: &str = "cursorByokCompletedToolMessages";
const REPLAY_ENVELOPE_PREFIX: &str = "cursor-byok:v1:";

/// Call ids already resolved in a pending assistant, whichever direction converts it.
pub(super) fn completed_call_ids(
    messages: &[crate::model::CanonicalMessage],
) -> std::collections::HashSet<String> {
    messages
        .iter()
        .filter_map(|message| match &message.content {
            crate::model::MessageContent::ToolResult(result) => Some(result.call_id.clone()),
            _ => None,
        })
        .collect()
}

#[cfg(test)]
mod tests;
