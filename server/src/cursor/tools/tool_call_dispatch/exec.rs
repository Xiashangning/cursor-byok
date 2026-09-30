//! Dispatches command execution Tool calls.
//! Direct Exec and dynamic MCP dispatch.

use crate::{cursor::protocol::proto::agent::v1 as pb, model::ToolCall, Error, Result};

use super::{normalized, ToolStart};
use crate::cursor::tools::{
    codec,
    runtime::{CursorToolRuntime, ExecContext},
    tool_call_result as result,
};

pub(super) async fn start(
    runtime: &CursorToolRuntime,
    call: &ToolCall,
    context: &ExecContext,
) -> Result<ToolStart> {
    let mut message = match normalized(&call.name).as_str() {
        "getmcptools" => codec::mcp_state_request(0, call),
        "callmcptool" => {
            let server = required(call, "server")?;
            let tool = required(call, "toolName")?;
            let Some(route) = context
                .mcp_routes
                .get(&(server.to_string(), tool.to_string()))
            else {
                return Ok(ToolStart {
                    messages: Vec::new(),
                    completion: Some(result::mcp_failure(
                        call,
                        format!("MCP descriptor not found for {server}/{tool}"),
                    )?),
                });
            };
            codec::mcp_meta_request(0, call, server, route)?
        }
        _ => codec::request(0, call, context)?,
    };
    if normalized(&call.name) == "task" {
        let selected_context = codec::task_attachments(call).await?;
        let Some(pb::agent_server_message::Message::ExecServerMessage(exec)) =
            message.message.as_mut()
        else {
            return Err(Error::Protocol("Task has no Exec request".into()));
        };
        let Some(pb::exec_server_message::Message::SubagentArgs(args)) = exec.message.as_mut()
        else {
            return Err(Error::Protocol("Task has no SubagentArgs request".into()));
        };
        args.selected_context = selected_context;
    }
    let id = runtime.reserve_exec(call, context).await?;
    if let Some(pb::agent_server_message::Message::ExecServerMessage(exec)) = &mut message.message {
        exec.id = id;
    }
    Ok(ToolStart {
        messages: vec![message],
        completion: None,
    })
}

fn required<'a>(call: &'a ToolCall, name: &str) -> Result<&'a str> {
    call.arguments
        .get(name)
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| Error::Protocol(format!("{} is missing {name}", call.name)))
}

pub(super) async fn start_dynamic(
    runtime: &CursorToolRuntime,
    call: &ToolCall,
    definition: &pb::McpToolDefinition,
    context: &ExecContext,
) -> Result<ToolStart> {
    let id = runtime
        .reserve_dynamic_mcp(call, context, definition)
        .await?;
    Ok(ToolStart {
        messages: vec![codec::mcp_request(id, call, definition)?],
        completion: None,
    })
}
