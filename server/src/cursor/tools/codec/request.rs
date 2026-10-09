//! Encodes Tool execution requests sent to Cursor.
use serde_json::{Map, Value};

use crate::{
    cursor::{
        protocol::proto::agent::v1 as pb,
        tools::{
            edit::{self, EditWrite},
            runtime::{ExecContext, McpRoute},
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
    let optional_string = |name: &str| {
        call.arguments
            .get(name)
            .and_then(Value::as_str)
            .map(str::to_string)
    };
    let int = |name: &str| -> Result<Option<i32>> {
        let minimum = if call.name == "Read" && name == "offset" {
            i32::MIN as i64
        } else {
            0
        };
        integer(call, name, minimum, i32::MAX as i64).map(|value| value.map(|value| value as i32))
    };
    let message = match normalize(&call.name).as_str() {
        "shell" => {
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
                output_notification: None,
                smart_mode_approval: smart_mode_approval(
                    call,
                    "request_smart_mode_approval",
                    "smart_mode_block_reason",
                )?,
                requested_sandbox_policy: shell_sandbox_policy(call),
                close_stdin: true,
                conversation_id: Some(context.conversation_id.clone()),
                ..Default::default()
            })
        }
        "read" => Message::ReadArgs(pb::ReadArgs {
            path: string("path")?,
            tool_call_id: call.call_id.clone(),
            offset: int("offset")?,
            limit: integer(call, "limit", 0, i32::MAX as i64)?.map(|value| value as u32),
            encoding_hint: None,
        }),
        "delete" => Message::DeleteArgs(pb::DeleteArgs {
            path: string("path")?,
            tool_call_id: call.call_id.clone(),
        }),
        "grep" => Message::GrepArgs(pb::GrepArgs {
            pattern: string("pattern")?,
            path: optional_string("path"),
            glob: optional_string("glob"),
            output_mode: optional_string("output_mode"),
            context_before: int("-B")?,
            context_after: int("-A")?,
            context: int("-C")?,
            case_insensitive: call.arguments.get("-i").and_then(Value::as_bool),
            r#type: optional_string("type"),
            head_limit: int("head_limit")?,
            multiline: call.arguments.get("multiline").and_then(Value::as_bool),
            sort: optional_string("sort"),
            sort_ascending: call
                .arguments
                .get("sort_ascending")
                .and_then(Value::as_bool),
            tool_call_id: call.call_id.clone(),
            sandbox_policy: None,
            offset: int("offset")?,
        }),
        "glob" => Message::GrepArgs(pb::GrepArgs {
            pattern: String::new(),
            path: optional_string("target_directory"),
            glob: optional_string("glob_pattern"),
            output_mode: Some("files_with_matches".into()),
            tool_call_id: call.call_id.clone(),
            ..Default::default()
        }),
        "task" => {
            let model_id = match call.arguments.get("model_selection") {
                Some(value) => {
                    serde_json::from_value::<crate::model::ModelSelection>(value.clone())?
                        .cursor_model_id(&context.model_directory)
                }
                None => string("model")?,
            };
            let resume_agent_id = optional_string("resume");
            Message::SubagentArgs(pb::SubagentArgs {
                tool_call_id: call.call_id.clone(),
                subagent_type: optional_string("subagent_type").unwrap_or_default(),
                model_id,
                prompt: string("prompt")?,
                readonly: false,
                resume_agent_id,
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
                selected_context: None,
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
                credentials: None,
            })
        }
        "sendmessagetoagent" => {
            let prompt = string("prompt")?;
            let agent_id = string("agent_id")?;
            if agent_id.trim().is_empty() {
                return Err(Error::Protocol(
                    "SendMessageToAgent agent_id must not be blank".into(),
                ));
            }
            // The client owns the resumed task. Preserve the existing public
            // follow-up wire defaults; do not accept hidden configuration overrides.
            // Whether the client honors these defaults when resuming needs client
            // verification; until then the follow-up runs as a plain agent.
            Message::SubagentArgs(pb::SubagentArgs {
                tool_call_id: call.call_id.clone(),
                subagent_type: String::new(),
                model_id: context.default_subagent_model.clone(),
                prompt,
                readonly: false,
                resume_agent_id: Some(agent_id),
                run_in_background: None,
                continuation_config: None,
                parent_conversation_id: Some(context.conversation_id.clone()),
                // follow-up 默认打断运行中的子代理；显式 false 保留忙时失败语义。
                interrupt: Some(
                    call.arguments
                        .get("interrupt")
                        .and_then(Value::as_bool)
                        .unwrap_or(true),
                ),
                mode: pb::TaskMode::Agent as i32,
                fork_agent_id: None,
                root_parent_conversation_id: Some(context.root_conversation_id.clone()),
                selected_context: None,
                direct_meta_parent_child_subagent: None,
                environment: pb::SubagentExecutionEnvironment::Local as i32,
                cloud_base_branch: None,
                credentials: None,
            })
        }
        "await" => {
            let args = crate::cursor::tools::runtime::await_arguments(call)?;
            Message::SubagentAwaitArgs(pb::SubagentAwaitArgs {
                agent_id: args.task_id,
                timeout_ms: args.block_until_ms.expect("Await timeout is resolved"),
            })
        }
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

pub(crate) fn shell_await_read_request(
    id: u32,
    call: &ToolCall,
    path: String,
) -> pb::AgentServerMessage {
    // Each poll is a fresh exec on the client, so it needs its own exec_id.
    server_message_with_exec_id(
        id,
        uuid::Uuid::new_v4().to_string(),
        pb::exec_server_message::Message::ReadArgs(pb::ReadArgs {
            path,
            tool_call_id: call.call_id.clone(),
            ..Default::default()
        }),
        Some(false),
    )
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

pub(crate) fn diagnostics_request(
    id: u32,
    call: &ToolCall,
    path: String,
) -> pb::AgentServerMessage {
    server_message(
        id,
        call,
        pb::exec_server_message::Message::DiagnosticsArgs(pb::DiagnosticsArgs {
            path,
            tool_call_id: call.call_id.clone(),
        }),
        Some(true),
    )
}

fn server_message(
    id: u32,
    call: &ToolCall,
    message: pb::exec_server_message::Message,
    accept_hook_additional_contexts: Option<bool>,
) -> pb::AgentServerMessage {
    server_message_with_exec_id(
        id,
        call.call_id.clone(),
        message,
        accept_hook_additional_contexts,
    )
}

fn server_message_with_exec_id(
    id: u32,
    exec_id: String,
    message: pb::exec_server_message::Message,
    accept_hook_additional_contexts: Option<bool>,
) -> pb::AgentServerMessage {
    pb::AgentServerMessage {
        ttft_breakdown: None,
        message: Some(pb::agent_server_message::Message::ExecServerMessage(
            pb::ExecServerMessage {
                id,
                exec_id,
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
    let args = match call.arguments.get("arguments") {
        None => Default::default(),
        Some(Value::Object(arguments)) => json_object_to_prost(arguments),
        Some(_) => {
            return Err(Error::Protocol(
                "CallMcpTool arguments must be a JSON object".into(),
            ));
        }
    };
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

pub(crate) fn integer(
    call: &ToolCall,
    name: &str,
    minimum: i64,
    maximum: i64,
) -> Result<Option<i64>> {
    call.arguments
        .get(name)
        .map(|value| {
            value
                .as_f64()
                .filter(|value| {
                    value.fract() == 0.0 && *value >= minimum as f64 && *value <= maximum as f64
                })
                .map(|value| value as i64)
                .ok_or_else(|| {
                    Error::Protocol(format!(
                        "{} {name} must be an integer in {minimum}..{maximum}",
                        call.name
                    ))
                })
        })
        .transpose()
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

/// Task 附件整体内联进一条流消息,重连依赖 64 MiB 的重放缓冲,因此单个
/// 附件与全部附件合计都不超过 32 MiB。
const MAX_TASK_ATTACHMENT_BYTES: u64 = 32 * 1024 * 1024;

pub(crate) async fn task_attachments(call: &ToolCall) -> Result<Option<pb::SelectedContext>> {
    let Some(paths) = call
        .arguments
        .get("file_attachments")
        .and_then(Value::as_array)
    else {
        return Ok(None);
    };
    if paths.is_empty() {
        return Ok(None);
    }
    let limit_mib = MAX_TASK_ATTACHMENT_BYTES / (1024 * 1024);
    let mut total_bytes = 0_u64;
    let mut context = pb::SelectedContext::default();
    for path in paths.iter().filter_map(Value::as_str) {
        let size = tokio::fs::metadata(path)
            .await
            .map_err(|error| {
                Error::Protocol(format!("cannot read Task attachment {path}: {error}"))
            })?
            .len();
        if size > MAX_TASK_ATTACHMENT_BYTES {
            return Err(Error::Protocol(format!(
                "Task attachment {path} exceeds the {limit_mib} MiB limit"
            )));
        }
        total_bytes = total_bytes.saturating_add(size);
        if total_bytes > MAX_TASK_ATTACHMENT_BYTES {
            return Err(Error::Protocol(format!(
                "Task attachments exceed the {limit_mib} MiB limit in total"
            )));
        }
        let data = tokio::fs::read(path).await.map_err(|error| {
            Error::Protocol(format!("cannot read Task attachment {path}: {error}"))
        })?;
        let file = std::path::Path::new(path);
        let filename = file
            .file_name()
            .and_then(std::ffi::OsStr::to_str)
            .unwrap_or_default()
            .to_string();
        let extension = file
            .extension()
            .and_then(std::ffi::OsStr::to_str)
            .unwrap_or_default()
            .to_ascii_lowercase();
        if let Some(mime_type) = video_mime_type(&extension) {
            context.selected_videos.push(pb::SelectedVideo {
                path: path.into(),
                filename,
                mime_type: mime_type.into(),
                materialize_to_filesystem: true,
                data_or_blob_id: Some(pb::selected_video::DataOrBlobId::Data(data)),
                ..Default::default()
            });
            continue;
        }
        let metadata = crate::model::image_metadata(&data).ok_or_else(|| {
            Error::Protocol(format!(
                "Task attachment is not a supported image or video: {path}"
            ))
        })?;
        context.selected_images.push(pb::SelectedImage {
            path: path.into(),
            mime_type: metadata.mime_type.into(),
            dimension: Some(pb::selected_image::Dimension {
                width: metadata.width,
                height: metadata.height,
            }),
            data_or_blob_id: Some(pb::selected_image::DataOrBlobId::Data(data)),
            ..Default::default()
        });
    }
    Ok(Some(context))
}

fn video_mime_type(extension: &str) -> Option<&'static str> {
    match extension {
        "mp4" => Some("video/mp4"),
        "mov" => Some("video/quicktime"),
        "webm" => Some("video/webm"),
        "mkv" => Some("video/x-matroska"),
        _ => None,
    }
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
    fn read_delete_canonical_path_matches_execution_and_display() {
        for name in ["Read", "Delete"] {
            for arguments in [json!({"path": "/primary"}), json!({"path": "/second"})] {
                let call = call(name, arguments);
                let expected = call.arguments.get("path").unwrap().as_str().unwrap();
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
    fn followup_requires_a_nonblank_canonical_agent_id() {
        for arguments in [
            json!({"prompt": "continue"}),
            json!({"agentId": "alias", "prompt": "continue"}),
            json!({"agent_id": "", "prompt": "continue"}),
            json!({"agent_id": " \t", "prompt": "continue"}),
            json!({"agent_id": null, "prompt": "continue"}),
        ] {
            assert!(request(
                7,
                &call("SendMessageToAgent", arguments),
                &ExecContext::default()
            )
            .is_err());
        }
    }

    #[test]
    fn mcp_arguments_default_to_empty_when_omitted() {
        let route = McpRoute {
            name: "search".into(),
            provider_identifier: "provider".into(),
            tool_name: "search".into(),
            input_schema: None,
        };
        for arguments in [
            json!({"server": "server", "toolName": "search"}),
            json!({"server": "server", "toolName": "search", "arguments": {}}),
        ] {
            assert!(mcp_meta_request(1, &call("CallMcpTool", arguments), "server", &route).is_ok());
        }
    }

    #[test]
    fn resumed_task_keeps_model_parameters_off_the_wire() {
        let model_directory = {
            let model = crate::model::ModelConfig {
                model_hash: "model-hash".into(),
                display_name: "Model".into(),
                model_id: "upstream-model".into(),
                effort_options: vec!["low".into(), "high".into()],
                context_options: vec!["200k".into(), "1m".into()],
                ..Default::default()
            };
            crate::model::ModelDirectory::new(std::slice::from_ref(&model), &[]).unwrap()
        };
        fn subagent_args(
            call: &ToolCall,
            model_directory: &crate::model::ModelDirectory,
        ) -> pb::SubagentArgs {
            let server = request(
                7,
                call,
                &crate::cursor::tools::runtime::ExecContext {
                    model_directory: model_directory.clone(),
                    ..ExecContext::default()
                },
            )
            .unwrap();
            let Some(pb::agent_server_message::Message::ExecServerMessage(server)) = server.message
            else {
                panic!("expected ExecServerMessage")
            };
            let Some(pb::exec_server_message::Message::SubagentArgs(args)) = server.message else {
                panic!("expected SubagentArgs")
            };
            args
        }

        // 续接已有子任务:参数不上线,保存的变体经 model_selection 烘焙进
        // model_id slug(派发侧准备的调用同时携带 model 与 model_selection)。
        // 参数仍出现在模型侧 JSON 参数里,由派发侧消费,编码忽略。
        let resumed = subagent_args(
            &call(
                "Task",
                json!({
                    "prompt": "continue",
                    "resume": "agent-1",
                    "model": "model-hash",
                    "model_parameters": [{"id": "reasoning", "value": "low"}],
                    "model_selection": {"kind":"local","id":"model-hash","parameters":{"context":"1m","reasoning":"low","fast":true}}
                }),
            ),
            &model_directory,
        );
        assert_eq!(resumed.resume_agent_id.as_deref(), Some("agent-1"));
        assert_eq!(resumed.model_id, "model-hash-1m-low-fast");

        // 无变体选择时裸 model 字符串原样透传。
        let plain = subagent_args(
            &call(
                "Task",
                json!({
                    "prompt": "continue",
                    "resume": "agent-1",
                    "model": "model-hash-1m-low"
                }),
            ),
            &model_directory,
        );
        assert_eq!(plain.model_id, "model-hash-1m-low");

        // resume=self 派生新代理,与创建一样:变体同样只进 model_id slug。
        for resume in [serde_json::json!("self"), serde_json::json!(null)] {
            let mut arguments = json!({
                "prompt": "inspect",
                "model": "model-hash",
                "model_selection": {"kind":"local","id":"model-hash","parameters":{"context":"1m","reasoning":"low","fast":false}}
            });
            if !resume.is_null() {
                arguments["resume"] = resume;
            }
            let args = subagent_args(&call("Task", arguments), &model_directory);
            assert_eq!(args.model_id, "model-hash-1m-low");
        }
    }

    #[test]
    fn send_message_to_agent_ignores_hidden_model_overrides() {
        let context = crate::cursor::tools::runtime::ExecContext {
            conversation_id: "conversation-1".into(),
            root_conversation_id: "root-1".into(),
            default_subagent_model: "model-1".into(),
            ..Default::default()
        };
        let server = request(
            7,
            &call(
                "SendMessageToAgent",
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
        assert_eq!(args.model_id, "model-1");
    }

    #[test]
    fn orchestration_tools_encode_to_client_exec_messages() {
        assert!(matches!(
            message(&call(
                "SendMessageToAgent",
                json!({"agent_id":"agent-1","prompt":"continue","readonly":true})
            )),
            pb::exec_server_message::Message::SubagentArgs(args)
                if args.resume_agent_id.as_deref() == Some("agent-1")
                    && args.parent_conversation_id.as_deref() == Some("conversation-1")
                    && args.mode == pb::TaskMode::Agent as i32
                    && args.interrupt == Some(true)
        ));
        // 显式 interrupt:false 保留忙时失败语义。
        assert!(matches!(
            message(&call(
                "SendMessageToAgent",
                json!({"agent_id":"agent-1","prompt":"continue","interrupt":false})
            )),
            pb::exec_server_message::Message::SubagentArgs(args) if args.interrupt == Some(false)
        ));
        // Task 未显式 interrupt 时不打断。
        assert!(matches!(
            message(&call(
                "Task",
                json!({"prompt":"inspect","resume":"agent-1","model":"m"})
            )),
            pb::exec_server_message::Message::SubagentArgs(args) if args.interrupt.is_none()
        ));
        assert!(matches!(
            message(&call("Await", json!({"task_id":"agent-1","block_until_ms":5000}))),
            pb::exec_server_message::Message::SubagentAwaitArgs(args)
                if args.agent_id == "agent-1" && args.timeout_ms == 5000
        ));
    }
}
