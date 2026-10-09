//! Dispatches Tool calls to their execution adapters.
mod await_shell;
mod edit;
mod exec;
mod interaction;
mod local;
mod search;

use std::collections::BTreeMap;

use crate::{
    cursor::protocol::proto::agent::v1 as pb,
    model::{CanonicalMessage, ToolCall},
    search::{WebFetch, WebSearch},
    store::Store,
    Result,
};

use super::{
    compat,
    runtime::{CursorToolRuntime, ExecContext, PendingInteraction},
    tool_call_result::{ToolCompletion, ToolResultSender},
};

pub(crate) use await_shell::{
    advance_poll as advance_shell_await_poll, complete_shell_awaits, MatchedShellAwaits,
    ShellAwaitPoll,
};

pub(super) struct ToolStart {
    pub messages: Vec<pb::AgentServerMessage>,
    pub completion: Option<ToolCompletion>,
}

pub(super) enum InteractionContinuation {
    Completed(Box<ToolCompletion>),
    Pending,
}

pub(super) async fn start(
    runtime: &CursorToolRuntime,
    results: &ToolResultSender,
    call: &ToolCall,
    message_index: usize,
    dynamic_mcp: &BTreeMap<String, pb::McpToolDefinition>,
    context: &ExecContext,
    store: Option<&Store>,
    messages: &[CanonicalMessage],
) -> Result<ToolStart> {
    if let Some(definition) = dynamic_mcp.get(&call.name) {
        return exec::start_dynamic(runtime, call, definition, context).await;
    }

    if is_mcp_auth(call) {
        return interaction::start(runtime, call).await;
    }

    if context.task_disabled(call) {
        return local::subagents_disabled(call);
    }

    match normalized(&call.name).as_str() {
        "readlints" => Ok(ToolStart {
            messages: vec![super::diagnostics::start(runtime, call, context).await?],
            completion: None,
        }),
        "shell" | "read" | "delete" | "grep" | "glob" | "task" | "sendmessagetoagent"
        | "callmcptool" | "fetchmcpresource" | "getmcptools" => {
            exec::start(runtime, call, context).await
        }
        "await" if call.arguments.get("shell_id").is_some() => {
            await_shell::start(runtime, results, call, context, messages).await
        }
        "await" => exec::start(runtime, call, context).await,
        "write" | "strreplace" | "editnotebook" => edit::start(runtime, call, context).await,
        "askquestion" | "websearch" | "webfetch" | "switchmode" | "createplan" => {
            interaction::start(runtime, call).await
        }
        "updatecurrentstep" => local::start(call, message_index),
        "semblesearch" | "semblefindrelated" => search::start(results, call, store.cloned()),
        _ => Ok(unavailable_tool(call)),
    }
}

fn unavailable_tool(call: &ToolCall) -> ToolStart {
    ToolStart {
        messages: Vec::new(),
        completion: Some(compat::failure(call)),
    }
}

fn is_mcp_auth(call: &ToolCall) -> bool {
    normalized(&call.name) == "callmcptool"
        && call
            .arguments
            .get("toolName")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|tool| normalized(tool) == "mcpauth")
}

pub(super) async fn resume_interaction(
    results: &ToolResultSender,
    search: &WebSearch,
    fetch: &WebFetch,
    pending: PendingInteraction,
    response: &pb::InteractionResponse,
) -> Result<InteractionContinuation> {
    interaction::resume(results, search, fetch, pending, response).await
}

pub(super) fn normalized(name: &str) -> String {
    name.chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}
#[cfg(test)]
mod tests {
    use super::*;

    fn tool(name: &str, arguments: serde_json::Value) -> ToolCall {
        ToolCall {
            index: 0,
            call_id: "call-1".into(),
            model_call_id: "model-call-1".into(),
            name: name.into(),
            arguments_text: arguments.to_string(),
            arguments,
            argument_error: None,
        }
    }

    #[test]
    fn arbitrary_unknown_tool_does_not_become_a_protocol_error() {
        let call = tool(
            "AwaitShell",
            serde_json::json!({"shell_id": "legacy-shell", "block_until_ms": 30_000}),
        );
        let started = unavailable_tool(&call);
        let completion = started.completion.expect("compatibility completion");

        assert!(started.messages.is_empty());
        assert!(completion.result().is_error);
        assert!(completion
            .result()
            .content
            .contains("current advertised tool set"));
    }
}
