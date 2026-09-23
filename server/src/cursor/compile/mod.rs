//! Compiles Cursor requests and actions into provider-independent Run inputs.

mod action;
mod break_messages;
mod context;
mod images;
mod insert_messages;
mod model;
mod run;

pub use action::*;
pub(crate) use break_messages::{compile_injection, compile_user_message_action, RuntimeAction};
pub use run::*;

/// 测试入口:投影一批后台完成通知,返回是否仍有待投影的完成项。
#[cfg(test)]
pub(crate) fn project_background_completion_for_test(
    action: &crate::cursor::protocol::proto::agent::v1::BackgroundTaskCompletionAction,
    suppressed: &std::collections::HashSet<String>,
) -> crate::Result<bool> {
    let projection = insert_messages::project(
        action,
        crate::cursor::protocol::proto::agent::v1::AgentMode::Agent as i32,
        suppressed,
    )?;
    Ok(projection.is_some())
}
