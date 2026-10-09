//! Tool contracts are enforced before execution, while continuations retain one call identity.
mod support;
use base64::{engine::general_purpose::STANDARD, Engine};
use cursor_server::{
    cursor::{
        prompting::Mode,
        protocol::proto::agent::v1 as pb,
        tools::{
            codec,
            runtime::{CursorToolRuntime, ExecContext},
            ToolBatchState, ToolDispatcher,
        },
    },
    model::{ToolCall, ToolDefinition},
};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashSet};
use support::tool_call;

fn call(name: &str, arguments: Value) -> ToolCall {
    tool_call(&format!("call-{name}"), name, arguments, "model-call")
}
fn context() -> ExecContext {
    let assets = support::prompt_assets();
    let mut tools = assets.mode(Mode::Agent).tools.clone();
    for mode in [Mode::Plan, Mode::Subagent] {
        for tool in &assets.mode(mode).tools {
            if !tools.iter().any(|existing| existing.name == tool.name) {
                tools.push(tool.clone());
            }
        }
    }
    ExecContext {
        tool_definitions: tools,
        default_subagent_model: "model".into(),
        allow_subagents: true,
        ..Default::default()
    }
}
async fn dispatch(
    dispatcher: &ToolDispatcher,
    calls: &[ToolCall],
    context: &ExecContext,
    dynamic: &BTreeMap<String, pb::McpToolDefinition>,
) -> Vec<cursor_server::cursor::tools::DispatchedTool> {
    dispatcher
        .start_batch(
            calls,
            ToolBatchState {
                completed: &HashSet::new(),
                started: &HashSet::new(),
                response_text: "",
                response_thinking: "",
            },
            &[],
            dynamic,
            context,
        )
        .await
        .unwrap()
}
fn exec(message: &pb::AgentServerMessage) -> &pb::ExecServerMessage {
    let Some(pb::agent_server_message::Message::ExecServerMessage(exec)) = &message.message else {
        panic!("expected exec")
    };
    exec
}

#[tokio::test]
async fn invalid_arguments_never_reserve_execute_approve_or_block_edits() {
    let runtime = CursorToolRuntime::default();
    let dispatcher = ToolDispatcher::new(runtime.clone());
    let cases = [
        (
            "Task",
            json!({"description":"Inspect", "prompt":"Inspect", "model_parameters":{"id":"reasoning", "value":"low"}}),
        ),
        (
            "Shell",
            json!({"command":"pwd", "required_permissions":["network"]}),
        ),
        ("Bash", json!({"command":"pwd"})),
        (
            "Shell",
            json!({"command":"pwd", "block_until_ms":2147483648u64}),
        ),
        ("Read", json!({"file_path":"removed-alias"})),
        ("Await", json!({"task_id":"t", "agentId":"alias"})),
        (
            "Shell",
            json!({"command":"pwd", "required_permissions":[42]}),
        ),
        (
            "SembleSearch",
            json!({"repo":"/repo", "query":"test", "top_k":101}),
        ),
        (
            "Task",
            json!({"description":"Inspect", "prompt":"Inspect", "model_parameters":{"id":"reasoning", "value":"low"}}),
        ),
        ("Grep", json!({"pattern":"x", "head_limit":2147483648u64})),
        ("Grep", json!({"pattern":"x", "offset":-1})),
        ("Grep", json!({"pattern":"x", "-A":1.5})),
        ("Grep", json!({"pattern":"x", "output_mode":"bogus"})),
        ("Grep", json!({"pattern":"x", "unknown":true})),
        ("Read", json!({"path":"a", "offset":-2147483649i64})),
        ("Read", json!({"path":"a", "limit":4294967296u64})),
        ("Write", json!({"path":"a", "contents":1})),
        (
            "StrReplace",
            json!({"path":"a", "old_string":"", "new_string":"x"}),
        ),
        (
            "StrReplace",
            json!({"path":"a", "old_string":"same\r\n", "new_string":"same\n"}),
        ),
        (
            "AskQuestion",
            json!({"questions":[{"id":"q", "prompt":"p", "options":[{"id":"a", "label":"A"}]}]}),
        ),
        (
            "CreatePlan",
            json!({"plan":"x", "overview":"x", "todos":[{"id":"a"}]}),
        ),
        (
            "EditNotebook",
            json!({"target_notebook":"a", "cell_idx":0, "new_string":"x"}),
        ),
        (
            "EditNotebook",
            json!({"target_notebook":"a", "cell_idx":0, "new_string":"x", "is_new_cell":true, "cell_language":"bogus"}),
        ),
        (
            "CallMcpTool",
            json!({"server":"s", "toolName":"mcp_auth", "arguments":null}),
        ),
        ("GetMcpTools", json!({"toolName":"x"})),
        ("GetMcpTools", json!({"pattern":"x".repeat(257)})),
        ("GetMcpTools", json!({"pattern":"(?=unsupported)"})),
        ("SwitchMode", json!({"target_mode_id":"debug"})),
        ("WebFetch", json!({"url":" "})),
        (
            "WebFetch",
            json!({"url":"https://example.com", "requestSmartModeApproval":true}),
        ),
        ("WebSearch", json!({"search_term":"\t"})),
        (
            "WebSearch",
            json!({"search_term":"x", "explanation":"removed"}),
        ),
        (
            "SendMessageToAgent",
            json!({"agent_id":"a", "prompt":"x", "model":"override"}),
        ),
        (
            "TodoWrite",
            json!({"merge":false, "todos":[{"id":"a", "content":"x", "status":"unknown"}]}),
        ),
        (
            "TodoWrite",
            json!({"merge":true, "todos":[{"id":"new", "status":"completed"}]}),
        ),
        ("ReadLints", json!({})),
        ("ReadLints", json!({"paths":[]})),
        ("ReadLints", json!({"paths":[""]})),
        ("ReadLints", json!({"paths":["   "]})),
        ("UpdateCurrentStep", json!({})),
        ("UpdateCurrentStep", json!({"final_summary":"done"})),
    ];
    for (name, arguments) in cases {
        let results = dispatch(
            &dispatcher,
            &[call(name, arguments)],
            &context(),
            &BTreeMap::new(),
        )
        .await;
        assert!(
            results[0].messages.is_empty(),
            "{name} emitted a side effect"
        );
        assert!(
            results[0].completion.as_ref().unwrap().result().is_error,
            "{name}"
        );
    }
    assert!(runtime.running_exec_ids().await.is_empty());
    let results = dispatch(
        &dispatcher,
        &[call("Write", json!({"path":"a", "contents":"ok"}))],
        &context(),
        &BTreeMap::new(),
    )
    .await;
    assert_eq!(
        exec(results[0].messages.last().unwrap()).id,
        1,
        "invalid calls must not allocate runtime ids or retain edit locks"
    );
}

/// Provider 产出的非法 JSON 参数在派发前转为工具错误:不下发客户端,也不占用运行时 id。
#[tokio::test]
async fn provider_argument_error_completes_without_client_exec() {
    let runtime = CursorToolRuntime::default();
    let dispatcher = ToolDispatcher::new(runtime.clone());
    let mut malformed = call("Read", json!({}));
    malformed.argument_error = Some("Read arguments are not valid JSON".into());

    let results = dispatch(&dispatcher, &[malformed], &context(), &BTreeMap::new()).await;

    assert!(
        results[0].messages.is_empty(),
        "malformed provider arguments must not reach the client"
    );
    let completion = results[0]
        .completion
        .as_ref()
        .expect("malformed arguments must complete as a tool error");
    assert!(completion.result().is_error);
    assert!(completion.result().content.contains("not valid JSON"));
    assert!(runtime.running_exec_ids().await.is_empty());
}

#[tokio::test]
async fn shell_await_starts_by_reading_the_window_terminal_file() {
    let runtime = CursorToolRuntime::default();
    let dispatcher = ToolDispatcher::new(runtime.clone());
    let mut context = context();
    context.terminals_folder = "/remote/home/user/.cursor/terminals".into();

    let started = dispatch(
        &dispatcher,
        &[call(
            "Await",
            json!({"shell_id":"42", "block_until_ms":50_000}),
        )],
        &context,
        &BTreeMap::new(),
    )
    .await;

    assert!(started[0].completion.is_none());
    let read = exec(started[0].messages.last().unwrap());
    assert_eq!(read.accept_hook_additional_contexts, Some(false));
    assert_ne!(read.exec_id, "call-Await");
    assert!(matches!(
        &read.message,
        Some(pb::exec_server_message::Message::ReadArgs(args))
            if args.path == "/remote/home/user/.cursor/terminals/42.txt"
                && args.tool_call_id == "call-Await"
    ));
}

#[tokio::test]
async fn shell_await_completes_from_the_terminal_read_result() {
    let runtime = CursorToolRuntime::default();
    let dispatcher = ToolDispatcher::new(runtime.clone());
    let mut context = context();
    context.terminals_folder = "/remote/home/user/.cursor/terminals".into();
    let started = dispatch(
        &dispatcher,
        &[call(
            "Await",
            json!({"shell_id":"42", "block_until_ms":50_000}),
        )],
        &context,
        &BTreeMap::new(),
    )
    .await;
    let read = exec(started[0].messages.last().unwrap());

    let event = codec::client_event(
        &pb::ExecClientMessage {
            id: read.id,
            message: Some(pb::exec_client_message::Message::ReadResult(
                pb::ReadResult {
                    result: Some(pb::read_result::Result::Success(pb::ReadSuccess {
                        path: "/remote/home/user/.cursor/terminals/42.txt".into(),
                        file_size: 273,
                        output: Some(pb::read_success::Output::Content(
                            "---\nstatus: succeeded\nrunning_for_ms: 31140\n---\nexit_code: 0\nelapsed_ms: 31140\nended_at: 2026-09-29T10:32:59.711Z\n---\n".into(),
                        )),
                        ..Default::default()
                    })),
                },
            )),
            ..Default::default()
        },
        &runtime,
    )
    .await
    .unwrap();

    let codec::ClientExecEvent::Completed(completion) = event else {
        panic!("terminal completion must finish Await")
    };
    assert_eq!(
        completion.result().content,
        "Task completed in 31140ms with exit code: 0.\noutput_file_path: /remote/home/user/.cursor/terminals/42.txt\noutput_length: 273"
    );
    let Some(pb::tool_call::Tool::AwaitToolCall(tool)) = &completion.tool_call().tool else {
        panic!("expected Await completion")
    };
    assert!(matches!(
        tool.result.as_ref().and_then(|result| result.result.as_ref()),
        Some(pb::await_result::Result::Success(pb::AwaitSuccess {
            await_result: Some(pb::await_success::AwaitResult::Complete(complete)),
        }))
            if complete.task_id == "42"
                && complete.runtime_ms == 31_140
                && complete.output_length == 273
                && complete.exit_code == Some(0)
    ));
    assert!(runtime.running_exec_ids().await.is_empty());
}

#[tokio::test]
async fn running_shell_await_schedules_another_read_with_new_exec_identity() {
    let runtime = CursorToolRuntime::default();
    let dispatcher = ToolDispatcher::new(runtime.clone());
    let mut context = context();
    context.terminals_folder = "/remote/home/user/.cursor/terminals".into();
    let started = dispatch(
        &dispatcher,
        &[call(
            "Await",
            json!({"shell_id":"42", "block_until_ms":50_000}),
        )],
        &context,
        &BTreeMap::new(),
    )
    .await;
    let first = exec(started[0].messages.last().unwrap());

    let event = codec::client_event(
        &pb::ExecClientMessage {
            id: first.id,
            message: Some(pb::exec_client_message::Message::ReadResult(
                pb::ReadResult {
                    result: Some(pb::read_result::Result::Success(pb::ReadSuccess {
                        path: "/remote/home/user/.cursor/terminals/42.txt".into(),
                        file_size: 198,
                        output: Some(pb::read_success::Output::Content(
                            "---\nstatus: running  \nrunning_for_ms: 2\n---\n".into(),
                        )),
                        ..Default::default()
                    })),
                },
            )),
            ..Default::default()
        },
        &runtime,
    )
    .await
    .unwrap();

    let codec::ClientExecEvent::DelayedMessage {
        delay,
        exec_id,
        message,
    } = event
    else {
        panic!("running terminal must schedule another read")
    };
    let next = exec(&message);
    assert_eq!(delay.as_millis(), 700);
    assert_eq!(exec_id, next.id);
    assert_ne!(next.id, first.id);
    assert_ne!(next.exec_id, first.exec_id);
    assert!(matches!(
        &next.message,
        Some(pb::exec_server_message::Message::ReadArgs(args))
            if args.path == "/remote/home/user/.cursor/terminals/42.txt"
                && args.tool_call_id == "call-Await"
    ));
}

#[tokio::test]
async fn str_replace_continues_from_normalized_read_to_write_with_one_exec_identity() {
    let runtime = CursorToolRuntime::default();
    let dispatcher = ToolDispatcher::new(runtime.clone());
    let started = dispatch(
        &dispatcher,
        &[call(
            "StrReplace",
            json!({"path":"/test.txt", "old_string":"a\r\nb", "new_string":"replacement"}),
        )],
        &context(),
        &BTreeMap::new(),
    )
    .await;
    let read = exec(started[0].messages.last().unwrap());
    assert!(matches!(
        &read.message,
        Some(pb::exec_server_message::Message::ReadArgs(args)) if args.path == "/test.txt"
    ));

    let event = codec::client_event(
        &pb::ExecClientMessage {
            id: read.id,
            message: Some(pb::exec_client_message::Message::ReadResult(
                pb::ReadResult {
                    result: Some(pb::read_result::Result::Success(pb::ReadSuccess {
                        path: "/test.txt".into(),
                        total_lines: 2,
                        file_size: 5,
                        output: Some(pb::read_success::Output::Content("a\nb\n".into())),
                        ..Default::default()
                    })),
                },
            )),
            ..Default::default()
        },
        &runtime,
    )
    .await
    .unwrap();
    let codec::ClientExecEvent::Message(write) = event else {
        panic!("normalized Read result must advance to Write")
    };
    let write = exec(&write);
    assert_ne!(write.id, read.id);
    assert_eq!(write.exec_id, read.exec_id);
    assert!(matches!(
        &write.message,
        Some(pb::exec_server_message::Message::WriteArgs(args))
            if args.path == "/test.txt" && args.file_text == "replacement\n"
    ));
}

/// 校验以本次运行下发的定义为准,而不是模式的静态工具表。
#[tokio::test]
async fn advertised_definitions_gate_validation() {
    let runtime = CursorToolRuntime::default();
    let dispatcher = ToolDispatcher::new(runtime);
    let mut context = context();
    context.tool_definitions = vec![ToolDefinition {
        name: "Task".into(),
        description: String::new(),
        parameters: json!({"type":"object", "properties":{"value":{"type":"number"}}, "required":["value"], "additionalProperties":false}),
    }];

    // Shell 是 Agent 模式工具,但不在本次运行下发的定义里。
    let results = dispatch(
        &dispatcher,
        &[call("Shell", json!({"command":"pwd"}))],
        &context,
        &BTreeMap::new(),
    )
    .await;
    assert!(results[0].messages.is_empty());
    assert!(results[0].completion.as_ref().unwrap().result().is_error);
}

#[tokio::test]
async fn todo_noop_merge_clear_and_duplicate_ids_are_explicit() {
    let dispatcher = ToolDispatcher::new(CursorToolRuntime::default());
    let mut context = context();
    let initial =
        json!({"merge":false,"todos":[{"id":"a","content":"Keep original","status":"pending"}]});
    context.initial_todos = Some(initial.clone());
    let mut noop = call("TodoWrite", json!({"merge":true,"todos":[]}));
    noop.call_id = "noop".into();
    let results = dispatch(
        &dispatcher,
        &[noop, call("TodoWrite", json!({"merge":false,"todos":[]}))],
        &context,
        &BTreeMap::new(),
    )
    .await;
    let outputs = results
        .iter()
        .map(|result| {
            let result = result.completion.as_ref().unwrap().result();
            assert!(!result.is_error);
            serde_json::from_str::<Value>(&result.content).unwrap()
        })
        .collect::<Vec<_>>();
    assert_eq!(
        outputs[0], initial,
        "a no-op merge must return the state it started from"
    );
    assert_eq!(outputs[1]["todos"], json!([]));
    let invalid = call(
        "TodoWrite",
        json!({"merge":false,"todos":[{"id":"a","content":"x","status":"pending"},{"id":"a","content":"y","status":"pending"}]}),
    );
    assert!(
        dispatch(&dispatcher, &[invalid], &context, &BTreeMap::new()).await[0]
            .completion
            .as_ref()
            .unwrap()
            .result()
            .is_error
    );
}

#[tokio::test]
async fn prepared_task_parameters_are_not_revalidated_as_model_input_or_written_to_history() {
    let dispatcher = ToolDispatcher::new(CursorToolRuntime::default());
    let mut context = context();
    context.model_directory = model_directory("model", "Display Model", &["1m"], &["low", "high"]);
    let invocation = call(
        "Task",
        json!({"description":"Inspect", "prompt":"Inspect only", "subagent_type":"custom-reviewer", "model_parameters":[{"id":"reasoning", "value":"low"}]}),
    );
    let original = invocation.clone();
    let results = dispatch(
        &dispatcher,
        std::slice::from_ref(&invocation),
        &context,
        &BTreeMap::new(),
    )
    .await;
    assert!(results[0].completion.is_none());
    let Some(pb::exec_server_message::Message::SubagentArgs(args)) =
        &exec(results[0].messages.last().unwrap()).message
    else {
        panic!("expected task")
    };
    assert_eq!(args.model_id, "model-1m-low");
    assert_eq!(args.subagent_type, "custom-reviewer");
    assert_eq!(invocation, original);
    assert!(invocation.arguments.get("model_display").is_none());
}

/// 构造单模型的统一目录(测试辅助)。
fn model_directory(
    id: &str,
    display_name: &str,
    context_options: &[&str],
    effort_options: &[&str],
) -> cursor_server::model::ModelDirectory {
    let model = cursor_server::model::ModelConfig {
        model_hash: id.into(),
        display_name: display_name.into(),
        model_id: format!("upstream-{id}"),
        effort_options: effort_options.iter().map(|value| (*value).into()).collect(),
        context_options: context_options
            .iter()
            .map(|value| (*value).into())
            .collect(),
        ..Default::default()
    };
    cursor_server::model::ModelDirectory::new(std::slice::from_ref(&model), &[]).unwrap()
}

/// 未匹配模型的 Task 调用原样透传客户端,由官方上游报告错误。
#[tokio::test]
async fn unmatched_task_models_pass_through_to_the_client() {
    let dispatcher = ToolDispatcher::new(CursorToolRuntime::default());
    let context = context();
    let invocation = call(
        "Task",
        json!({"description":"Inspect", "prompt":"Inspect", "model":"official-unknown-model"}),
    );
    let results = dispatch(
        &dispatcher,
        std::slice::from_ref(&invocation),
        &context,
        &BTreeMap::new(),
    )
    .await;
    // 无派发期拒绝:请求正常下发给客户端执行。
    assert!(results[0].completion.is_none());
    let Some(pb::exec_server_message::Message::SubagentArgs(args)) =
        &exec(results[0].messages.last().unwrap()).message
    else {
        panic!("expected task")
    };
    assert_eq!(args.model_id, "official-unknown-model");
}

#[tokio::test]
async fn inline_mcp_schema_is_checked_before_dispatch_but_path_only_schema_stays_client_owned() {
    let runtime = CursorToolRuntime::default();
    let dispatcher = ToolDispatcher::new(runtime.clone());
    let mut context = context();
    let key = ("server".into(), "remote".into());
    context.mcp_routes.insert(key.clone(), cursor_server::cursor::tools::runtime::McpRoute {
        name:"server-remote".into(), provider_identifier:"server".into(), tool_name:"remote".into(),
        input_schema: Some(json!({"type":"object", "required":["number"], "properties":{"number":{"type":"integer"}}}).to_string()),
    });
    let invocation = call(
        "CallMcpTool",
        json!({"server":"server", "toolName":"remote", "arguments":{"number":"wrong"}}),
    );
    let results = dispatch(
        &dispatcher,
        std::slice::from_ref(&invocation),
        &context,
        &BTreeMap::new(),
    )
    .await;
    assert!(results[0].messages.is_empty());
    assert!(results[0].completion.as_ref().unwrap().result().is_error);
    context.mcp_routes.get_mut(&key).unwrap().input_schema = None;
    let results = dispatch(&dispatcher, &[invocation], &context, &BTreeMap::new()).await;
    assert_eq!(exec(results[0].messages.last().unwrap()).id, 1);
}

#[tokio::test]
async fn task_attachments_are_complete_and_followups_keep_message_association() {
    let directory = tempfile::tempdir().unwrap();
    let image_path = directory.path().join("attachment.png");
    let video_path = directory.path().join("clip.mp4");
    let png = STANDARD
        .decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=")
        .unwrap();
    let video = b"video-data".to_vec();
    std::fs::write(&image_path, &png).unwrap();
    std::fs::write(&video_path, &video).unwrap();
    let image_path = image_path.to_string_lossy().into_owned();
    let video_path = video_path.to_string_lossy().into_owned();
    let task = call(
        "Task",
        json!({
            "description":"Title",
            "prompt":"Prompt",
            "model":"model",
            "file_attachments":[image_path.clone(), video_path.clone()]
        }),
    );
    let rendered = codec::render_tool_call(&task, false).unwrap();
    let Some(pb::tool_call::Tool::TaskToolCall(tool)) = rendered.tool else {
        panic!("expected task card")
    };
    let args = tool.args.unwrap();
    assert_eq!(args.description, "Title");
    assert_eq!(args.prompt, "Prompt");
    assert_eq!(args.attachments, [image_path.clone(), video_path.clone()]);

    let runtime = CursorToolRuntime::default();
    let dispatcher = ToolDispatcher::new(runtime.clone());
    let results = dispatch(&dispatcher, &[task], &context(), &BTreeMap::new()).await;
    let request = exec(results[0].messages.last().unwrap());
    let Some(pb::exec_server_message::Message::SubagentArgs(args)) = &request.message else {
        panic!("expected SubagentArgs")
    };
    let selected = args.selected_context.as_ref().unwrap();
    let [image] = selected.selected_images.as_slice() else {
        panic!("expected one selected image")
    };
    assert_eq!(image.path, image_path);
    assert_eq!(image.mime_type, "image/png");
    assert_eq!(
        image
            .dimension
            .as_ref()
            .map(|value| (value.width, value.height)),
        Some((1, 1))
    );
    assert!(matches!(
        image.data_or_blob_id.as_ref(),
        Some(pb::selected_image::DataOrBlobId::Data(data)) if data == &png
    ));
    let [selected_video] = selected.selected_videos.as_slice() else {
        panic!("expected one selected video")
    };
    assert_eq!(selected_video.path, video_path);
    assert_eq!(selected_video.filename, "clip.mp4");
    assert_eq!(selected_video.mime_type, "video/mp4");
    assert!(selected_video.materialize_to_filesystem);
    assert!(matches!(
        selected_video.data_or_blob_id.as_ref(),
        Some(pb::selected_video::DataOrBlobId::Data(data)) if data == &video
    ));

    let followup = call(
        "SendMessageToAgent",
        json!({"agent_id":"a", "prompt":"Continue", "responding_to_message_ids":["message-1"]}),
    );
    let rendered = codec::render_tool_call(&followup, false).unwrap();
    let Some(pb::tool_call::Tool::TaskToolCall(tool)) = rendered.tool else {
        panic!("expected task card")
    };
    assert_eq!(tool.args.unwrap().responding_to_message_ids, ["message-1"]);
}

fn diagnostics(path: &str, failed: bool) -> pb::exec_client_message::Message {
    pb::exec_client_message::Message::DiagnosticsResult(pb::DiagnosticsResult {
        result: Some(if failed {
            pb::diagnostics_result::Result::Error(pb::DiagnosticsError {
                path: path.into(),
                error: format!("cannot inspect {path}"),
            })
        } else {
            pb::diagnostics_result::Result::Success(pb::DiagnosticsSuccess {
                path: path.into(),
                diagnostics: vec![pb::Diagnostic {
                    message: format!("warning in {path}"),
                    severity: pb::DiagnosticSeverity::Warning as i32,
                    ..Default::default()
                }],
                total_diagnostics: 1,
            })
        }),
    })
}

#[tokio::test]
async fn read_lints_checks_every_path_and_aggregates_success_or_error_once() {
    for failing_path in [None, Some("a.ts")] {
        let failed = failing_path.is_some();
        let runtime = CursorToolRuntime::default();
        let dispatcher = ToolDispatcher::new(runtime.clone());
        let invocation = call("ReadLints", json!({"paths":["a.ts","b.ts"]}));
        let results = dispatch(
            &dispatcher,
            std::slice::from_ref(&invocation),
            &context(),
            &BTreeMap::new(),
        )
        .await;
        let first = exec(results[0].messages.last().unwrap());
        assert_eq!(first.accept_hook_additional_contexts, Some(true));
        assert!(
            matches!(&first.message, Some(pb::exec_server_message::Message::DiagnosticsArgs(args)) if args.path == "a.ts")
        );
        let event = codec::client_event(
            &pb::ExecClientMessage {
                id: first.id,
                message: Some(diagnostics("a.ts", failing_path == Some("a.ts"))),
                ..Default::default()
            },
            &runtime,
        )
        .await
        .unwrap();
        let codec::ClientExecEvent::Message(next) = event else {
            panic!("first path must not complete the tool")
        };
        let next = exec(&next);
        assert_eq!(next.accept_hook_additional_contexts, Some(true));
        assert_ne!(first.id, next.id);
        assert_eq!(next.exec_id, first.exec_id);
        assert!(
            matches!(&next.message, Some(pb::exec_server_message::Message::DiagnosticsArgs(args)) if args.path == "b.ts")
        );
        let event = codec::client_event(
            &pb::ExecClientMessage {
                id: next.id,
                message: Some(diagnostics("b.ts", failing_path == Some("b.ts"))),
                ..Default::default()
            },
            &runtime,
        )
        .await
        .unwrap();
        let codec::ClientExecEvent::Completed(completion) = event else {
            panic!("expected one aggregate completion")
        };
        assert_eq!(completion.result().is_error, failed);
        assert_eq!(completion.result().call_id, invocation.call_id);
        assert!(completion.result().content.contains("a.ts"));
        assert!(completion.result().content.contains("b.ts"));
        let Some(pb::tool_call::Tool::ReadLintsToolCall(tool)) = &completion.tool_call().tool
        else {
            panic!("wrong result card")
        };
        assert_eq!(tool.args.as_ref().unwrap().paths.len(), 2);
        if !failed {
            assert!(
                matches!(&tool.result.as_ref().unwrap().result, Some(pb::read_lints_tool_result::Result::Success(success)) if success.total_files == 2 && success.total_diagnostics == 2)
            );
        }
        assert!(runtime.running_exec_ids().await.is_empty());
    }
}

#[tokio::test]
async fn numeric_integer_floats_are_coerced_before_execution() {
    let dispatcher = ToolDispatcher::new(CursorToolRuntime::default());
    for (name, arguments) in [
        ("Grep", json!({"pattern":"x","-A":2.0,"offset":2147483647})),
        (
            "Read",
            json!({"path":"a","offset":-2147483648i64,"limit":2.0}),
        ),
        ("Shell", json!({"command":"pwd","block_until_ms":3000.0})),
    ] {
        let results = dispatch(
            &dispatcher,
            &[call(name, arguments)],
            &context(),
            &BTreeMap::new(),
        )
        .await;
        assert!(results[0].completion.is_none());
        match &exec(results[0].messages.last().unwrap()).message {
            Some(pb::exec_server_message::Message::GrepArgs(args)) => {
                assert_eq!(args.context_after, Some(2));
                assert_eq!(args.offset, Some(i32::MAX));
            }
            Some(pb::exec_server_message::Message::ReadArgs(args)) => {
                assert_eq!(args.offset, Some(i32::MIN));
                assert_eq!(args.limit, Some(2));
            }
            Some(pb::exec_server_message::Message::ShellStreamArgs(args)) => {
                assert_eq!(args.timeout, 3_000);
            }
            _ => panic!("unexpected request"),
        }
    }
}
