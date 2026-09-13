//! Encodes Tool execution requests sent to Cursor.
use serde_json::{Map, Value};

use crate::{
    cursor::{
        protocol::proto::agent::v1 as pb,
        tools::{
            edit::{self, EditWrite},
            runtime::{parse_task_model_parameters, ExecContext, McpRoute},
        },
    },
    model::ToolCall,
    Error, Result,
};

pub fn request(id: u32, call: &ToolCall, context: &ExecContext) -> Result<pb::AgentServerMessage> {
    use pb::exec_server_message::Message;
    let string = |name: &str| {
        call.arguments
            .get(name)
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| Error::Protocol(format!("{} is missing {name}", call.name)))
    };
    // Claude 系模型常按 Claude Code 习惯输出别名参数(如 file_path),逐个回退兼容。
    let string_aliased = |names: &[&str]| -> Result<String> {
        names
            .iter()
            .find_map(|name| call.arguments.get(name))
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| Error::Protocol(format!("{} is missing {}", call.name, names[0])))
    };
    let optional_string = |name: &str| {
        call.arguments
            .get(name)
            .and_then(Value::as_str)
            .map(str::to_string)
    };
    let int = |name: &str| {
        call.arguments
            .get(name)
            .and_then(Value::as_i64)
            .map(|v| v as i32)
    };
    let message = match normalize(&call.name).as_str() {
        "shell" | "bash" => {
            let command = string("command")?;
            let (simple_commands, parsing_result) = shell_command_metadata(&command);
            Message::ShellStreamArgs(pb::ShellArgs {
                command,
                working_directory: optional_string("working_directory").unwrap_or_default(),
                timeout: shell_timeout(call)?,
                tool_call_id: call.call_id.clone(),
                simple_commands,
                parsing_result,
                file_output_threshold_bytes: Some(40_000),
                timeout_behavior: pb::TimeoutBehavior::Background as i32,
                hard_timeout: Some(86_400_000),
                description: optional_string("description"),
                output_notification: shell_notification(call)?,
                smart_mode_approval: smart_mode_approval(
                    call,
                    "request_smart_mode_approval",
                    "smart_mode_block_reason",
                )?,
                requested_sandbox_policy: shell_sandbox_policy(call),
                close_stdin: true,
                conversation_id: Some(context.conversation_id.clone()),
                admin_command_denylist: context.admin_command_denylist.clone(),
                ..Default::default()
            })
        }
        "read" => Message::ReadArgs(pb::ReadArgs {
            path: string_aliased(&["path", "file_path", "filePath"])?,
            tool_call_id: call.call_id.clone(),
            offset: int("offset"),
            limit: call
                .arguments
                .get("limit")
                .and_then(Value::as_u64)
                .map(|v| v as u32),
            encoding_hint: optional_string("encoding_hint"),
        }),
        "delete" => Message::DeleteArgs(pb::DeleteArgs {
            path: string_aliased(&["path", "file_path", "filePath"])?,
            tool_call_id: call.call_id.clone(),
        }),
        "grep" => Message::GrepArgs(pb::GrepArgs {
            pattern: string("pattern")?,
            path: optional_string("path"),
            glob: optional_string("glob"),
            output_mode: optional_string("output_mode"),
            context_before: int("-B"),
            context_after: int("-A"),
            context: int("-C"),
            case_insensitive: call.arguments.get("-i").and_then(Value::as_bool),
            r#type: optional_string("type"),
            head_limit: int("head_limit"),
            multiline: call.arguments.get("multiline").and_then(Value::as_bool),
            sort: optional_string("sort"),
            sort_ascending: call
                .arguments
                .get("sort_ascending")
                .and_then(Value::as_bool),
            tool_call_id: call.call_id.clone(),
            sandbox_policy: None,
            offset: int("offset"),
        }),
        "glob" => Message::GrepArgs(pb::GrepArgs {
            pattern: String::new(),
            path: optional_string("target_directory"),
            glob: optional_string("glob_pattern"),
            output_mode: Some("files_with_matches".into()),
            tool_call_id: call.call_id.clone(),
            ..Default::default()
        }),
        "readlints" => Message::DiagnosticsArgs(pb::DiagnosticsArgs {
            path: call
                .arguments
                .get("paths")
                .and_then(Value::as_array)
                .and_then(|paths| paths.first())
                .and_then(Value::as_str)
                .unwrap_or_default()
                .into(),
            tool_call_id: call.call_id.clone(),
        }),
        "task" => {
            let model_parameters = task_model_parameters(call)?;
            let model_id = string("model")?;
            Message::SubagentArgs(pb::SubagentArgs {
                tool_call_id: call.call_id.clone(),
                subagent_type: optional_string("subagent_type").unwrap_or_default(),
                model_id,
                prompt: string("prompt")?,
                readonly: false,
                resume_agent_id: optional_string("resume"),
                run_in_background: call
                    .arguments
                    .get("run_in_background")
                    .and_then(Value::as_bool),
                continuation_config: None,
                parent_conversation_id: Some(context.conversation_id.clone()),
                interrupt: call.arguments.get("interrupt").and_then(Value::as_bool),
                mode: 0,
                fork_agent_id: None,
                root_parent_conversation_id: Some(context.root_conversation_id.clone()),
                selected_context: task_attachments(call),
                direct_meta_parent_child_subagent: None,
                environment: match optional_string("environment").as_deref() {
                    Some("cloud") => pb::SubagentExecutionEnvironment::Cloud as i32,
                    Some("local") | None => pb::SubagentExecutionEnvironment::Local as i32,
                    Some(value) => {
                        return Err(Error::Protocol(format!(
                            "unknown Task environment: {value}"
                        )))
                    }
                },
                cloud_base_branch: optional_string("cloud_base_branch"),
                model_parameters,
                credentials: None,
            })
        }
        "createagent" => Message::ForceBackgroundSubagentArgs(pb::ForceBackgroundSubagentArgs {
            tool_call_id: call.call_id.clone(),
        }),
        "sendmessagetoagent" => {
            let prompt = string("prompt")?;
            let agent_id = optional_string("agent_id").or_else(|| optional_string("agentId"));
            let requested_model = optional_string("model").filter(|model| !model.is_empty());
            let subagent_type = optional_string("subagent_type")
                .or_else(|| optional_string("subagentType"))
                .unwrap_or_default();
            let model_id = match context.subagent_model_for(&subagent_type) {
                Some(crate::cursor::tools::runtime::SubagentModel::Model(model)) => model.clone(),
                Some(crate::cursor::tools::runtime::SubagentModel::Inherit) => {
                    context.default_subagent_model.clone()
                }
                Some(crate::cursor::tools::runtime::SubagentModel::Disabled) => {
                    return Err(Error::Protocol(
                        "send-message-to-agent is disabled by the subagent model override".into(),
                    ))
                }
                None => requested_model
                    .map(|model| context.canonical_model(&model))
                    .unwrap_or_else(|| context.default_subagent_model.clone()),
            };
            let readonly = call
                .arguments
                .get("readonly")
                .or_else(|| call.arguments.get("readOnly"))
                .and_then(Value::as_bool)
                .unwrap_or(false);
            Message::SubagentArgs(pb::SubagentArgs {
                tool_call_id: call.call_id.clone(),
                subagent_type,
                model_id,
                prompt,
                readonly,
                resume_agent_id: agent_id,
                run_in_background: None,
                continuation_config: None,
                parent_conversation_id: Some(context.conversation_id.clone()),
                interrupt: None,
                mode: if readonly {
                    pb::TaskMode::Plan as i32
                } else {
                    pb::TaskMode::Agent as i32
                },
                fork_agent_id: None,
                root_parent_conversation_id: Some(context.root_conversation_id.clone()),
                selected_context: None,
                direct_meta_parent_child_subagent: None,
                environment: pb::SubagentExecutionEnvironment::Local as i32,
                cloud_base_branch: None,
                model_parameters: Vec::new(),
                credentials: None,
            })
        }
        "await" => Message::SubagentAwaitArgs(pb::SubagentAwaitArgs {
            agent_id: optional_string("task_id")
                .or_else(|| optional_string("agent_id"))
                .or_else(|| optional_string("agentId"))
                .ok_or_else(|| Error::Protocol("AWAIT is missing task_id".into()))?,
            timeout_ms: call
                .arguments
                .get("block_until_ms")
                .or_else(|| call.arguments.get("timeout_ms"))
                .and_then(Value::as_u64)
                .unwrap_or(30_000)
                .min(u32::MAX as u64) as u32,
        }),
        "fetchmcpresource" => Message::ReadMcpResourceExecArgs(pb::ReadMcpResourceExecArgs {
            server: string("server")?,
            uri: string("uri")?,
            download_path: optional_string("downloadPath"),
            tool_call_id: call.call_id.clone(),
            smart_mode_approval: smart_mode_approval(
                call,
                "requestSmartModeApproval",
                "smartModeBlockReason",
            )?,
        }),
        other => {
            return Err(Error::Protocol(format!(
                "tool {other} is not executed through ExecServerMessage"
            )))
        }
    };
    let accept_hook_additional_contexts =
        if matches!(&message, pb::exec_server_message::Message::SubagentArgs(_)) {
            Some(false)
        } else {
            Some(true)
        };
    Ok(server_message(
        id,
        call,
        message,
        accept_hook_additional_contexts,
    ))
}

pub(crate) fn edit_read_request(id: u32, call: &ToolCall) -> Result<pb::AgentServerMessage> {
    Ok(server_message(
        id,
        call,
        pb::exec_server_message::Message::ReadArgs(pb::ReadArgs {
            path: edit::path(call)?,
            tool_call_id: call.call_id.clone(),
            ..Default::default()
        }),
        Some(true),
    ))
}

pub(super) fn edit_write_request(
    id: u32,
    call: &ToolCall,
    write: &EditWrite,
) -> Result<pb::AgentServerMessage> {
    Ok(server_message(
        id,
        call,
        pb::exec_server_message::Message::WriteArgs(pb::WriteArgs {
            path: edit::path(call)?,
            file_text: write.after.clone(),
            tool_call_id: call.call_id.clone(),
            return_file_content_after_write: false,
            file_bytes: Vec::new(),
            encoding_hint: None,
        }),
        Some(true),
    ))
}

fn server_message(
    id: u32,
    call: &ToolCall,
    message: pb::exec_server_message::Message,
    accept_hook_additional_contexts: Option<bool>,
) -> pb::AgentServerMessage {
    pb::AgentServerMessage {
        ttft_breakdown: None,
        message: Some(pb::agent_server_message::Message::ExecServerMessage(
            pb::ExecServerMessage {
                id,
                exec_id: call.call_id.clone(),
                span_context: None,
                accept_hook_additional_contexts,
                message: Some(message),
            },
        )),
    }
}

pub fn mcp_request(
    id: u32,
    call: &ToolCall,
    definition: &pb::McpToolDefinition,
) -> Result<pb::AgentServerMessage> {
    let args = call
        .arguments
        .as_object()
        .map(json_object_to_prost)
        .unwrap_or_default();
    Ok(pb::AgentServerMessage {
        ttft_breakdown: None,
        message: Some(pb::agent_server_message::Message::ExecServerMessage(
            pb::ExecServerMessage {
                id,
                exec_id: call.call_id.clone(),
                span_context: None,
                accept_hook_additional_contexts: None,
                message: Some(pb::exec_server_message::Message::McpArgs(pb::McpArgs {
                    name: definition.name.clone(),
                    args,
                    tool_call_id: call.call_id.clone(),
                    provider_identifier: definition.provider_identifier.clone(),
                    tool_name: definition.tool_name.clone(),
                    smart_mode_approval: None,
                    smart_mode_approval_only: false,
                    skip_approval: false,
                    server_identifier: String::new(),
                })),
            },
        )),
    })
}

pub(crate) fn mcp_meta_request(
    id: u32,
    call: &ToolCall,
    server_identifier: &str,
    route: &McpRoute,
) -> Result<pb::AgentServerMessage> {
    if route.name.is_empty() || route.provider_identifier.is_empty() || route.tool_name.is_empty() {
        return Err(Error::Protocol(format!(
            "MCP definition for {server_identifier} is incomplete"
        )));
    }
    let requested_tool = call
        .arguments
        .get("toolName")
        .and_then(Value::as_str)
        .ok_or_else(|| Error::Protocol("CallMcpTool is missing toolName".into()))?;
    if requested_tool != route.tool_name {
        return Err(Error::Protocol(format!(
            "MCP definition mismatch: requested {requested_tool}, resolved {}",
            route.tool_name
        )));
    }
    let args = call
        .arguments
        .get("arguments")
        .and_then(Value::as_object)
        .map(json_object_to_prost)
        .unwrap_or_default();
    Ok(server_message(
        id,
        call,
        pb::exec_server_message::Message::McpArgs(pb::McpArgs {
            name: route.name.clone(),
            args,
            tool_call_id: call.call_id.clone(),
            provider_identifier: route.provider_identifier.clone(),
            tool_name: route.tool_name.clone(),
            smart_mode_approval: smart_mode_approval(
                call,
                "requestSmartModeApproval",
                "smartModeBlockReason",
            )?,
            smart_mode_approval_only: false,
            skip_approval: false,
            server_identifier: server_identifier.into(),
        }),
        Some(true),
    ))
}

pub fn mcp_state_request(id: u32, call: &ToolCall) -> pb::AgentServerMessage {
    let server_identifiers = call
        .arguments
        .get("server")
        .and_then(Value::as_str)
        .map(|server| vec![server.into()])
        .unwrap_or_default();
    server_message(
        id,
        call,
        pb::exec_server_message::Message::McpStateExecArgs(pb::McpStateExecArgs {
            server_identifiers,
            kick_only: false,
        }),
        Some(false),
    )
}

pub fn abort(id: u32) -> pb::AgentServerMessage {
    pb::AgentServerMessage {
        ttft_breakdown: None,
        message: Some(pb::agent_server_message::Message::ExecServerControlMessage(
            pb::ExecServerControlMessage {
                message: Some(pb::exec_server_control_message::Message::Abort(
                    pb::ExecServerAbort { id },
                )),
            },
        )),
    }
}

fn shell_sandbox_policy(call: &ToolCall) -> Option<pb::SandboxPolicy> {
    let permissions = call.arguments.get("required_permissions")?.as_array()?;
    let perms: Vec<&str> = permissions.iter().filter_map(Value::as_str).collect();
    if perms.contains(&"all") {
        Some(pb::SandboxPolicy {
            r#type: pb::sandbox_policy::Type::InsecureNone as i32,
            network_access: Some(true),
            ..Default::default()
        })
    } else if perms.contains(&"full_network") {
        Some(pb::SandboxPolicy {
            r#type: pb::sandbox_policy::Type::WorkspaceReadwrite as i32,
            network_access: Some(true),
            ..Default::default()
        })
    } else {
        None
    }
}

fn shell_command_metadata(command: &str) -> (Vec<String>, Option<pb::ShellCommandParsingResult>) {
    let command = command.trim();
    let mut parts = command.split_whitespace();
    let Some(name) = parts.next() else {
        return (Vec::new(), None);
    };
    let args = parts
        .map(
            |value| pb::shell_command_parsing_result::ExecutableCommandArg {
                r#type: "word".into(),
                value: value.into(),
            },
        )
        .collect();
    (
        vec![command.into()],
        Some(pb::ShellCommandParsingResult {
            executable_commands: vec![pb::shell_command_parsing_result::ExecutableCommand {
                name: name.into(),
                args,
                full_text: command.into(),
            }],
            ..Default::default()
        }),
    )
}

fn shell_timeout(call: &ToolCall) -> Result<i32> {
    let value = call
        .arguments
        .get("block_until_ms")
        .map(|value| {
            value
                .as_i64()
                .ok_or_else(|| Error::Protocol("Shell block_until_ms must be an integer".into()))
        })
        .transpose()?
        .unwrap_or(30_000);
    i32::try_from(value)
        .ok()
        .filter(|value| *value >= 0)
        .ok_or_else(|| Error::Protocol("Shell block_until_ms is out of range".into()))
}

fn smart_mode_approval(
    call: &ToolCall,
    request_field: &str,
    reason_field: &str,
) -> Result<Option<pb::SmartModeApproval>> {
    if !call
        .arguments
        .get(request_field)
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return Ok(None);
    }
    let reason = call
        .arguments
        .get(reason_field)
        .and_then(Value::as_str)
        .ok_or_else(|| Error::Protocol(format!("{} requires {reason_field}", call.name)))?;
    Ok(Some(pb::SmartModeApproval {
        request_id: call.call_id.clone(),
        reason: reason.to_string(),
    }))
}

fn shell_notification(call: &ToolCall) -> Result<Option<pb::ShellOutputNotificationConfig>> {
    let Some(value) = call.arguments.get("notify_on_output") else {
        return Ok(None);
    };
    let object = value
        .as_object()
        .ok_or_else(|| Error::Protocol("Shell notify_on_output must be an object".into()))?;
    let required = |field: &str| {
        object
            .get(field)
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| Error::Protocol(format!("Shell notify_on_output is missing {field}")))
    };
    Ok(Some(pb::ShellOutputNotificationConfig {
        pattern: required("pattern")?,
        reason: required("reason")?,
        debounce: object.get("debounce_ms").and_then(Value::as_f64),
        notification_limit: None,
    }))
}

fn task_model_parameters(call: &ToolCall) -> Result<Vec<pb::requested_model::ModelParameterValue>> {
    let arguments = call
        .arguments
        .as_object()
        .ok_or_else(|| Error::Protocol("Task arguments must be a JSON object".into()))?;
    Ok(parse_task_model_parameters(arguments)?
        .into_iter()
        .map(|(id, value)| pb::requested_model::ModelParameterValue { id, value })
        .collect())
}

fn task_attachments(call: &ToolCall) -> Option<pb::SelectedContext> {
    let paths = call.arguments.get("file_attachments")?.as_array()?;
    let mut context = pb::SelectedContext::default();
    for path in paths.iter().filter_map(Value::as_str) {
        let extension = std::path::Path::new(path)
            .extension()
            .and_then(std::ffi::OsStr::to_str)
            .unwrap_or_default()
            .to_ascii_lowercase();
        if matches!(extension.as_str(), "mp4" | "mov" | "webm" | "mkv") {
            context.selected_videos.push(pb::SelectedVideo {
                path: path.into(),
                filename: std::path::Path::new(path)
                    .file_name()
                    .and_then(std::ffi::OsStr::to_str)
                    .unwrap_or_default()
                    .into(),
                materialize_to_filesystem: true,
                ..Default::default()
            });
        } else {
            context.selected_images.push(pb::SelectedImage {
                path: path.into(),
                ..Default::default()
            });
        }
    }
    Some(context)
}

fn normalize(value: &str) -> String {
    value
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

pub(crate) fn json_object_to_prost(
    value: &Map<String, Value>,
) -> std::collections::HashMap<String, prost_types::Value> {
    value
        .iter()
        .map(|(key, value)| (key.clone(), prost_value(value)))
        .collect()
}

fn prost_value(value: &Value) -> prost_types::Value {
    use prost_types::{value::Kind, ListValue, Struct, Value as ProstValue};
    let kind = match value {
        Value::Null => Kind::NullValue(0),
        Value::Bool(v) => Kind::BoolValue(*v),
        Value::Number(v) => Kind::NumberValue(v.as_f64().unwrap_or_default()),
        Value::String(v) => Kind::StringValue(v.clone()),
        Value::Array(v) => Kind::ListValue(ListValue {
            values: v.iter().map(prost_value).collect(),
        }),
        Value::Object(v) => Kind::StructValue(Struct {
            fields: json_object_to_prost(v).into_iter().collect(),
        }),
    };
    ProstValue { kind: Some(kind) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn call(name: &str, arguments: Value) -> ToolCall {
        ToolCall {
            index: 0,
            call_id: "call-1".into(),
            model_call_id: "model-1".into(),
            name: name.into(),
            arguments_text: arguments.to_string(),
            arguments,
            argument_error: None,
        }
    }

    fn message(call: &ToolCall) -> pb::exec_server_message::Message {
        let server = request(
            7,
            call,
            &crate::cursor::tools::runtime::ExecContext {
                conversation_id: "conversation-1".into(),
                root_conversation_id: "root-1".into(),
                default_subagent_model: "model-1".into(),
                ..Default::default()
            },
        )
        .unwrap();
        let Some(pb::agent_server_message::Message::ExecServerMessage(server)) = server.message
        else {
            panic!("expected ExecServerMessage")
        };
        server.message.unwrap()
    }

    #[test]
    fn read_delete_aliases_match_execution_and_display() {
        for name in ["Read", "Delete"] {
            for arguments in [
                json!({"file_path": "/snake"}),
                json!({"filePath": "/camel"}),
                json!({"path": "/primary", "file_path": "/snake", "filePath": "/camel"}),
                json!({"file_path": "/snake", "filePath": "/camel"}),
            ] {
                let call = call(name, arguments);
                let expected = call
                    .arguments
                    .get("path")
                    .or_else(|| call.arguments.get("file_path"))
                    .or_else(|| call.arguments.get("filePath"))
                    .unwrap()
                    .as_str()
                    .unwrap();
                let path = match message(&call) {
                    pb::exec_server_message::Message::ReadArgs(args) => args.path,
                    pb::exec_server_message::Message::DeleteArgs(args) => args.path,
                    _ => panic!("unexpected execution"),
                };
                assert_eq!(path, expected);
                let rendered = super::super::render_tool_call(&call, false).unwrap();
                let displayed = match rendered.tool.unwrap() {
                    pb::tool_call::Tool::ReadToolCall(tool) => tool.args.unwrap().path,
                    pb::tool_call::Tool::DeleteToolCall(tool) => tool.args.unwrap().path,
                    _ => panic!("unexpected display"),
                };
                assert_eq!(displayed, path);
            }
            for value in [Value::Null, json!(42), json!([])] {
                let call = call(name, json!({"path": value, "file_path": "/alias"}));
                assert!(request(7, &call, &ExecContext::default()).is_err());
            }
        }
    }

    #[test]
    fn task_model_parameters_accepts_single_object_parameter() {
        let parameters = task_model_parameters(&call(
            "Task",
            json!({
                "model_parameters": {"id": "reasoning", "value": "low"}
            }),
        ))
        .unwrap();
        assert_eq!(parameters.len(), 1);
        assert_eq!(parameters[0].id, "reasoning");
        assert_eq!(parameters[0].value, "low");
    }

    #[test]
    fn task_model_parameters_reject_duplicate_and_unknown_ids() {
        let duplicate = call(
            "Task",
            json!({
                "model_parameters": [
                    {"id": "context", "value": "200k"},
                    {"id": "context", "value": "1m"}
                ]
            }),
        );
        assert!(matches!(
            task_model_parameters(&duplicate),
            Err(Error::Protocol(message)) if message.contains("repeats context")
        ));
        let unknown = call(
            "Task",
            json!({"model_parameters": [{"id": "effort", "value": "low"}]}),
        );
        assert!(matches!(
            task_model_parameters(&unknown),
            Err(Error::Protocol(message)) if message.contains("reasoning or context")
        ));
    }

    #[test]
    fn send_message_to_agent_normalizes_the_model_through_aliases() {
        let mut context = crate::cursor::tools::runtime::ExecContext {
            conversation_id: "conversation-1".into(),
            root_conversation_id: "root-1".into(),
            default_subagent_model: "model-1".into(),
            ..Default::default()
        };
        context
            .model_directory
            .aliases
            .insert("deepseek flash".into(), "hash-deepseek".into());
        let server = request(
            7,
            &call(
                "send-message-to-agent",
                json!({"agent_id":"agent-1","prompt":"continue","model":"DeepSeek Flash"}),
            ),
            &context,
        )
        .unwrap();
        let Some(pb::agent_server_message::Message::ExecServerMessage(server)) = server.message
        else {
            panic!("expected ExecServerMessage")
        };
        let Some(pb::exec_server_message::Message::SubagentArgs(args)) = server.message else {
            panic!("expected SubagentArgs")
        };
        assert_eq!(args.model_id, "hash-deepseek");
    }

    #[test]
    fn orchestration_tools_encode_to_client_exec_messages() {
        assert!(matches!(
            message(&call("create-agent", json!({"title":"Inspect","prompt":"inspect"}))),
            pb::exec_server_message::Message::ForceBackgroundSubagentArgs(args)
                if args.tool_call_id == "call-1"
        ));
        assert!(matches!(
            message(&call(
                "send-message-to-agent",
                json!({"agent_id":"agent-1","prompt":"continue","readonly":true})
            )),
            pb::exec_server_message::Message::SubagentArgs(args)
                if args.resume_agent_id.as_deref() == Some("agent-1")
                    && args.parent_conversation_id.as_deref() == Some("conversation-1")
                    && args.mode == pb::TaskMode::Plan as i32
        ));
        assert!(matches!(
            message(&call("AWAIT", json!({"task_id":"agent-1","block_until_ms":5000}))),
            pb::exec_server_message::Message::SubagentAwaitArgs(args)
                if args.agent_id == "agent-1" && args.timeout_ms == 5000
        ));
    }

    #[test]
    fn bash_is_encoded_as_a_shell_exec_request() {
        // The dispatcher routes `bash`/`Bash` to the shell executor, so the
        // request codec must encode it as a Shell stream instead of erroring
        // with `tool bash is not executed through ExecServerMessage`.
        let pb::exec_server_message::Message::ShellStreamArgs(args) =
            message(&call("Bash", json!({ "command": "ls -la" })))
        else {
            panic!("expected ShellStreamArgs");
        };
        assert_eq!(args.command, "ls -la");
    }
}
