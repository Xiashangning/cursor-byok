//! Runs every explicit ReadLints file sequentially under one tool-call lifecycle.
use super::{
    codec::{self, ClientExecEvent},
    runtime::{CursorToolRuntime, ExecContext, ExecStage, PendingExec},
    tool_call_result,
};
use crate::{cursor::protocol::proto::agent::v1 as pb, model::ToolCall, Error, Result};
use std::collections::VecDeque;

pub(crate) struct DiagnosticsState {
    pub paths: VecDeque<String>,
    pub results: Vec<pb::DiagnosticsResult>,
}

/// 一次 Diagnostics 结果缺失时的统一失败表示;每条路径都必须给出可读错误。
pub(crate) fn failed_result(error: impl Into<String>) -> pb::DiagnosticsResult {
    pb::DiagnosticsResult {
        result: Some(pb::diagnostics_result::Result::Error(
            pb::DiagnosticsError {
                path: String::new(),
                error: error.into(),
            },
        )),
    }
}

pub(crate) async fn start(
    runtime: &CursorToolRuntime,
    call: &ToolCall,
    context: &ExecContext,
) -> Result<pb::AgentServerMessage> {
    let mut paths = call
        .arguments
        .get("paths")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(serde_json::Value::as_str)
        .filter(|path| !path.trim().is_empty())
        .map(str::to_owned)
        .collect::<VecDeque<_>>();
    let path = paths.pop_front().ok_or_else(|| {
        Error::Protocol("ReadLints requires at least one non-empty file path".into())
    })?;
    let id = runtime
        .reserve_diagnostics(
            call,
            context,
            DiagnosticsState {
                paths,
                results: Vec::new(),
            },
            None,
        )
        .await?;
    Ok(codec::diagnostics_request(id, call, path))
}

pub(crate) async fn advance(
    mut pending: PendingExec,
    result: pb::DiagnosticsResult,
    runtime: &CursorToolRuntime,
) -> Result<ClientExecEvent> {
    let ExecStage::Diagnostics(mut state) =
        std::mem::replace(&mut pending.stage, ExecStage::Direct)
    else {
        unreachable!()
    };
    state.results.push(if result.result.is_some() {
        result
    } else {
        failed_result("Diagnostics returned no result")
    });
    if let Some(path) = state.paths.pop_front() {
        let id = runtime
            .reserve_diagnostics(
                &pending.call,
                &pending.context,
                state,
                Some(pending.started_at_ms),
            )
            .await?;
        return Ok(ClientExecEvent::Message(Box::new(
            codec::diagnostics_request(id, &pending.call, path),
        )));
    }
    Ok(ClientExecEvent::Completed(Box::new(
        tool_call_result::complete_diagnostics(pending, &state.results)?,
    )))
}
