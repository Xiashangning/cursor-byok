//! Tracks running Tool executions and coordinates cancellation and cleanup.
use std::{
    collections::{HashMap, HashSet},
    sync::{
        atomic::{AtomicU32, Ordering},
        Arc,
    },
};

use tokio::sync::Mutex;

use crate::{
    cursor::protocol::proto::agent::v1 as pb,
    model::{ModelVariantAxis, ModelVariantParts, SubagentKind, ToolCall},
    Error, Result,
};

use super::edit::EditWrite;

#[derive(Clone, Default)]
pub struct CursorToolRuntime {
    next_id: Arc<AtomicU32>,
    execs: Arc<Mutex<HashMap<u32, PendingExec>>>,
    interactions: Arc<Mutex<HashMap<u32, PendingInteraction>>>,
    completed: Arc<Mutex<HashMap<u32, String>>>,
    interrupted: Arc<Mutex<HashSet<u32>>>,
    shell_awaits: Arc<Mutex<HashMap<String, PendingShellAwait>>>,
}

#[derive(Clone)]
pub(crate) struct PendingShellAwait {
    pub call: ToolCall,
    pub shell_id: String,
    pub started_at_ms: u64,
}

pub(crate) struct PendingExec {
    pub call: ToolCall,
    pub context: ExecContext,
    pub started_at_ms: u64,
    pub stdout: String,
    pub stderr: String,
    pub stage: ExecStage,
}

pub(crate) enum ExecStage {
    Direct,
    DynamicMcp(pb::McpToolDefinition),
    Diagnostics(super::diagnostics::DiagnosticsState),
    EditRead,
    EditWrite(EditWrite),
    ShellAwaitPoll,
}

#[derive(Clone, Debug, Default)]
pub struct ExecContext {
    pub conversation_id: String,
    pub tool_definitions: Vec<crate::model::ToolDefinition>,
    pub initial_todos: Option<serde_json::Value>,
    pub root_conversation_id: String,
    pub default_subagent_model: String,
    pub default_subagent_model_variant: Option<String>,
    pub model_directory: ModelDirectory,
    pub subagent_models: HashMap<SubagentKind, SubagentModel>,
    pub allow_subagents: bool,
    pub terminals_folder: String,
    pub admin_command_denylist: Vec<String>,
    pub mcp_routes: HashMap<(String, String), McpRoute>,
}

/// 一次运行可见的模型目录:别名、变体轴与展示名,供编排工具解析模型选择。
#[derive(Clone, Debug, Default)]
pub struct ModelDirectory {
    pub aliases: HashMap<String, String>,
    pub variants: HashMap<String, ModelVariantAxis>,
    pub display_names: HashMap<String, String>,
}

impl ModelDirectory {
    /// 解析模型选择:变体 slug 归一到 base hash 并保留变体分量,别名归一到
    /// base hash,未知值原样透传(由回程解析报错)。
    pub(crate) fn resolve(&self, key: &str) -> (String, Option<ModelVariantParts>) {
        for (hash, axis) in &self.variants {
            if let Some(parts) = axis
                .parse_slug(hash, key)
                .or_else(|| axis.parse_slug(hash, &key.to_ascii_lowercase()))
            {
                return (hash.clone(), Some(parts));
            }
        }
        let canonical = self
            .aliases
            .get(key)
            .or_else(|| self.aliases.get(&key.to_ascii_lowercase()))
            .cloned()
            .unwrap_or_else(|| key.to_string());
        (canonical, None)
    }
}

#[derive(Clone, Debug)]
pub struct McpRoute {
    pub name: String,
    pub provider_identifier: String,
    pub tool_name: String,
    pub input_schema: Option<String>,
}

#[derive(Clone, Debug)]
pub enum SubagentModel {
    Model(String),
    Inherit,
    Disabled,
}

/// Tools that delegate work to subagents. The prompt omits them and the runtime
/// rejects them when subagents are disabled for a run.
pub(crate) fn is_orchestration_tool(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "task" | "sendmessagetoagent"
    )
}

pub(crate) fn subagent_kind(value: &str) -> SubagentKind {
    if value == "generalPurpose" {
        SubagentKind::GeneralPurpose
    } else {
        SubagentKind::Named(value.into())
    }
}

/// Task 的续接目标:非空且不是 `self`(self 派生新代理,按创建处理)。
/// 派发侧据此决定是否持久化变体,编码侧据此决定是否下发 model_parameters。
pub(crate) fn task_resume_target(call: &ToolCall) -> Option<&str> {
    if !call.name.eq_ignore_ascii_case("Task") {
        return None;
    }
    call.arguments
        .get("resume")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|target| !target.is_empty() && !target.eq_ignore_ascii_case("self"))
}

/// 续接请求是否显式选择了模型:带 `model` 键或非空 `model_parameters`。
/// 两者都缺省的续接只是沿用继承的父级默认值,不得把它固化成子会话配置。
pub(crate) fn task_resume_has_explicit_model(
    arguments: &serde_json::Map<String, serde_json::Value>,
) -> bool {
    arguments.contains_key("model")
        || arguments
            .get("model_parameters")
            .and_then(serde_json::Value::as_array)
            .is_some_and(|parameters| !parameters.is_empty())
}

/// 解析 Task 的 model_parameters:形状、id 白名单与重复 id 在此统一校验,
/// 值归一小写;值是否落在模型档位轴上由 prepare_call_with_resume_variant 对照变体轴校验。
pub(crate) fn parse_task_model_parameters(
    arguments: &serde_json::Map<String, serde_json::Value>,
) -> Result<Vec<(String, String)>> {
    let Some(value) = arguments.get("model_parameters") else {
        return Ok(Vec::new());
    };
    let entries: Vec<&serde_json::Value> = match value {
        serde_json::Value::Array(values) => values.iter().collect(),
        _ => {
            return Err(Error::Protocol(
                "Task model_parameters must be an array".into(),
            ))
        }
    };
    let mut parameters: Vec<(String, String)> = Vec::with_capacity(entries.len());
    for entry in entries {
        let object = entry.as_object().ok_or_else(|| {
            Error::Protocol("Task model_parameters entries must be objects".into())
        })?;
        if object
            .keys()
            .any(|key| !matches!(key.as_str(), "id" | "value"))
        {
            return Err(Error::Protocol(
                "Task model_parameters entries only accept id and value".into(),
            ));
        }
        let id = object
            .get("id")
            .and_then(serde_json::Value::as_str)
            .filter(|id| matches!(*id, "reasoning" | "context"))
            .ok_or_else(|| {
                Error::Protocol("Task model_parameters id must be reasoning or context".into())
            })?;
        let value = object
            .get("value")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                Error::Protocol(format!("Task model parameter {id} is missing value"))
            })?;
        if parameters.iter().any(|(existing, _)| existing == id) {
            return Err(Error::Protocol(format!(
                "Task model_parameters repeats {id}"
            )));
        }
        parameters.push((id.to_string(), value.trim().to_ascii_lowercase()));
    }
    Ok(parameters)
}

/// One canonical target and one checked timeout for both execution and cards.
pub(crate) fn await_arguments(call: &ToolCall) -> Result<pb::AwaitArgs> {
    let arguments = call
        .arguments
        .as_object()
        .ok_or_else(|| Error::Protocol("Await arguments must be a JSON object".into()))?;
    if arguments
        .keys()
        .any(|key| !matches!(key.as_str(), "shell_id" | "task_id" | "block_until_ms"))
    {
        return Err(Error::Protocol(
            "Await accepts only shell_id, task_id and block_until_ms".into(),
        ));
    }
    let target = match (arguments.get("shell_id"), arguments.get("task_id")) {
        (Some(target), None) | (None, Some(target)) => target,
        _ => {
            return Err(Error::Protocol(
                "Await accepts exactly one of shell_id or task_id".into(),
            ))
        }
    };
    let target = target
        .as_str()
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| Error::Protocol("Await target must be a non-blank string".into()))?;
    let timeout = match arguments.get("block_until_ms") {
        None => 30_000,
        Some(value) => {
            let value = value
                .as_f64()
                .filter(|value| {
                    value.fract() == 0.0 && *value >= 0.0 && *value <= f64::from(u32::MAX)
                })
                .ok_or_else(|| {
                    Error::Protocol(
                        "Await block_until_ms must be an integer in 0..4294967295".into(),
                    )
                })?;
            value as u32
        }
    };
    Ok(pb::AwaitArgs {
        task_id: target.into(),
        block_until_ms: Some(timeout),
        regex: None,
    })
}

impl ExecContext {
    pub(crate) fn subagent_model_for(&self, subagent_type: &str) -> Option<&SubagentModel> {
        self.subagent_models.get(&subagent_kind(subagent_type))
    }

    pub fn task_disabled(&self, call: &ToolCall) -> bool {
        if !is_orchestration_tool(&call.name) {
            return false;
        }
        let subagent_type = call
            .arguments
            .get("subagent_type")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("generalPurpose");
        matches!(
            self.subagent_model_for(subagent_type),
            Some(SubagentModel::Disabled)
        )
    }

    pub(crate) fn prepare_call_with_resume_variant(
        &self,
        call: &ToolCall,
        resume_model_variant: Option<&str>,
    ) -> Result<ToolCall> {
        if !call.name.eq_ignore_ascii_case("Task") {
            return Ok(call.clone());
        }
        let arguments = call
            .arguments
            .as_object()
            .ok_or_else(|| Error::Protocol("Task arguments must be a JSON object".into()))?;
        if let Some(kind) = arguments
            .get("subagent_type")
            .and_then(serde_json::Value::as_str)
        {
            if kind.trim().is_empty() {
                return Err(Error::Protocol(
                    "Task subagent_type must not be blank".into(),
                ));
            }
        }
        let subagent_type = arguments
            .get("subagent_type")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("generalPurpose");
        if self.task_disabled(call) {
            return Ok(call.clone());
        }
        let override_model = self.subagent_model_for(subagent_type);
        let inherited = || {
            self.default_subagent_model_variant
                .clone()
                .unwrap_or_else(|| self.default_subagent_model.clone())
        };
        let requested_model = arguments.get("model").and_then(serde_json::Value::as_str);
        // 续接任务拥有已保存配置。父级模型默认值只初始化新任务，不应静默
        // 重置已有任务；只有显式 model 参数才更改续接任务的模型选择。
        let model = match (resume_model_variant, requested_model) {
            (Some(saved), None) => saved.to_owned(),
            _ => match override_model {
                Some(SubagentModel::Model(model)) => model.clone(),
                Some(SubagentModel::Inherit) => inherited(),
                Some(SubagentModel::Disabled) => unreachable!("disabled Task returned above"),
                None => match requested_model {
                    Some(model) if model.eq_ignore_ascii_case("inherit") => inherited(),
                    Some(model) => model.to_owned(),
                    None => inherited(),
                },
            },
        };
        let parameters = parse_task_model_parameters(arguments)?;
        let (base, parts) = self.model_directory.resolve(&model);
        if override_model.is_none()
            && arguments
                .get("model")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|model| !model.eq_ignore_ascii_case("inherit"))
            && base != self.model_directory.resolve(&inherited()).0
            && !self.model_directory.display_names.contains_key(&base)
            && !self.model_directory.variants.contains_key(&base)
        {
            return Err(Error::Protocol(format!(
                "Task model is not available: {model}"
            )));
        }
        let mut display_name = None;
        let model = match self.model_directory.variants.get(&base) {
            Some(axis) => {
                display_name = self.model_directory.display_names.get(&base).cloned();
                // 默认变体（新建时继承父任务，续接时使用保存配置）或显式
                // 变体 slug 为基线，LLM 显式参数逐项覆盖。
                let mut effective = parts.clone().or_else(|| axis.default_parts());
                match effective.as_mut() {
                    Some(effective) => {
                        for (id, value) in &parameters {
                            match id.as_str() {
                                "context" => effective.context = axis.validate_context(value)?,
                                "reasoning" => {
                                    effective.effort = Some(axis.validate_effort(value)?)
                                }
                                _ => unreachable!("parameter ids are validated above"),
                            }
                        }
                        // 无参数且无显式变体时透传 base hash;其余情况烘焙变体 slug。
                        if parts.is_some() || !parameters.is_empty() {
                            axis.bake_slug(&base, effective)
                        } else {
                            base
                        }
                    }
                    None => base,
                }
            }
            None => base,
        };
        if model.is_empty() {
            return Err(Error::Protocol(format!(
                "Task subagent type {subagent_type} has no model"
            )));
        }
        let mut prepared = call.clone();
        let prepared_arguments = prepared
            .arguments
            .as_object_mut()
            .expect("Task arguments were validated");
        prepared_arguments.insert("model".into(), serde_json::Value::String(model));
        if let Some(display_name) = display_name {
            prepared_arguments.insert(
                "model_display".into(),
                serde_json::Value::String(display_name),
            );
        }
        Ok(prepared)
    }
}

pub(crate) struct PendingInteraction {
    pub call: ToolCall,
    pub started_at_ms: u64,
}

impl CursorToolRuntime {
    pub(crate) fn next_run(&self) -> Self {
        Self {
            next_id: self.next_id.clone(),
            execs: Arc::new(Mutex::new(HashMap::new())),
            interactions: Arc::new(Mutex::new(HashMap::new())),
            completed: Arc::new(Mutex::new(HashMap::new())),
            interrupted: self.interrupted.clone(),
            shell_awaits: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub(crate) async fn reserve_shell_await(
        &self,
        call: &ToolCall,
        shell_id: String,
    ) -> Result<PendingShellAwait> {
        let started_at_ms = now_ms();
        let pending = PendingShellAwait {
            call: call.clone(),
            shell_id: shell_id.clone(),
            started_at_ms,
        };
        let mut awaits = self.shell_awaits.lock().await;
        if awaits.contains_key(&shell_id) {
            return Err(Error::Protocol(format!(
                "shell {shell_id} already has a pending Await"
            )));
        }
        awaits.insert(shell_id, pending.clone());
        Ok(pending)
    }

    pub(crate) async fn reserve_shell_poll(
        &self,
        call: &ToolCall,
        context: &ExecContext,
        shell_id: &str,
        started_at_ms: u64,
    ) -> Result<Option<u32>> {
        let awaits = self.shell_awaits.lock().await;
        if !awaits.get(shell_id).is_some_and(|pending| {
            pending.call.call_id == call.call_id && pending.started_at_ms == started_at_ms
        }) {
            return Ok(None);
        }
        let id = self.next_id()?;
        self.execs.lock().await.insert(
            id,
            PendingExec {
                call: call.clone(),
                context: context.clone(),
                started_at_ms,
                stdout: String::new(),
                stderr: String::new(),
                stage: ExecStage::ShellAwaitPoll,
            },
        );
        drop(awaits);
        Ok(Some(id))
    }

    pub(crate) async fn take_shell_await(
        &self,
        shell_id: &str,
        call_id: &str,
    ) -> Option<PendingShellAwait> {
        let pending = {
            let mut awaits = self.shell_awaits.lock().await;
            if awaits
                .get(shell_id)
                .is_some_and(|pending| pending.call.call_id == call_id)
            {
                awaits.remove(shell_id)
            } else {
                None
            }
        };
        if pending.is_some() {
            self.discard_shell_polls(&HashSet::from([call_id.to_string()]))
                .await;
        }
        pending
    }

    pub(crate) async fn take_shell_awaits(
        &self,
        shell_ids: &[String],
    ) -> Option<Vec<PendingShellAwait>> {
        let pending = {
            let mut awaits = self.shell_awaits.lock().await;
            if shell_ids
                .iter()
                .any(|shell_id| !awaits.contains_key(shell_id))
            {
                return None;
            }
            shell_ids
                .iter()
                .filter_map(|shell_id| awaits.remove(shell_id))
                .collect::<Vec<_>>()
        };
        let call_ids = pending
            .iter()
            .map(|pending| pending.call.call_id.clone())
            .collect();
        self.discard_shell_polls(&call_ids).await;
        Some(pending)
    }

    async fn discard_shell_polls(&self, call_ids: &HashSet<String>) {
        let discarded = {
            let mut execs = self.execs.lock().await;
            let ids = execs
                .iter()
                .filter_map(|(id, entry)| {
                    (matches!(entry.stage, ExecStage::ShellAwaitPoll)
                        && call_ids.contains(&entry.call.call_id))
                    .then_some(*id)
                })
                .collect::<Vec<_>>();
            for id in &ids {
                execs.remove(id);
            }
            ids
        };
        self.interrupted.lock().await.extend(discarded);
    }

    pub async fn reserve_exec(&self, call: &ToolCall, context: &ExecContext) -> Result<u32> {
        self.reserve_exec_stage(call, context, ExecStage::Direct, None)
            .await
    }

    pub(crate) async fn reserve_diagnostics(
        &self,
        call: &ToolCall,
        context: &ExecContext,
        state: super::diagnostics::DiagnosticsState,
        started_at_ms: Option<u64>,
    ) -> Result<u32> {
        self.reserve_exec_stage(call, context, ExecStage::Diagnostics(state), started_at_ms)
            .await
    }

    pub(crate) async fn reserve_dynamic_mcp(
        &self,
        call: &ToolCall,
        context: &ExecContext,
        definition: &pb::McpToolDefinition,
    ) -> Result<u32> {
        self.reserve_exec_stage(
            call,
            context,
            ExecStage::DynamicMcp(definition.clone()),
            None,
        )
        .await
    }

    pub(crate) async fn reserve_edit_read(
        &self,
        call: &ToolCall,
        context: &ExecContext,
    ) -> Result<u32> {
        self.reserve_exec_stage(call, context, ExecStage::EditRead, None)
            .await
    }

    pub(crate) async fn reserve_edit_write(
        &self,
        call: &ToolCall,
        context: &ExecContext,
        write: EditWrite,
        started_at_ms: u64,
    ) -> Result<u32> {
        self.reserve_exec_stage(
            call,
            context,
            ExecStage::EditWrite(write),
            Some(started_at_ms),
        )
        .await
    }

    async fn reserve_exec_stage(
        &self,
        call: &ToolCall,
        context: &ExecContext,
        stage: ExecStage,
        started_at_ms: Option<u64>,
    ) -> Result<u32> {
        let id = self.next_id()?;
        self.execs.lock().await.insert(
            id,
            PendingExec {
                call: call.clone(),
                context: context.clone(),
                started_at_ms: started_at_ms.unwrap_or_else(now_ms),
                stdout: String::new(),
                stderr: String::new(),
                stage,
            },
        );
        Ok(id)
    }

    pub async fn reserve_interaction(&self, call: &ToolCall) -> Result<u32> {
        let id = self.next_id()?;
        self.interactions.lock().await.insert(
            id,
            PendingInteraction {
                call: call.clone(),
                started_at_ms: now_ms(),
            },
        );
        Ok(id)
    }

    pub async fn exec_call(&self, id: u32) -> Option<ToolCall> {
        self.execs
            .lock()
            .await
            .get(&id)
            .map(|entry| entry.call.clone())
    }

    pub async fn append_stdout(&self, id: u32, data: &str) -> bool {
        let mut entries = self.execs.lock().await;
        let Some(entry) = entries.get_mut(&id) else {
            return false;
        };
        entry.stdout.push_str(data);
        true
    }

    pub async fn append_stderr(&self, id: u32, data: &str) -> bool {
        let mut entries = self.execs.lock().await;
        let Some(entry) = entries.get_mut(&id) else {
            return false;
        };
        entry.stderr.push_str(data);
        true
    }

    pub(crate) async fn take_exec(&self, id: u32) -> Option<PendingExec> {
        let pending = self.execs.lock().await.remove(&id);
        if let Some(pending) = &pending {
            self.completed
                .lock()
                .await
                .insert(id, pending.call.call_id.clone());
        }
        pending
    }

    pub(crate) async fn take_interaction(&self, id: u32) -> Option<PendingInteraction> {
        let pending = self.interactions.lock().await.remove(&id);
        if let Some(pending) = &pending {
            self.completed
                .lock()
                .await
                .insert(id, pending.call.call_id.clone());
        }
        pending
    }

    pub async fn completed_call(&self, id: u32) -> Option<String> {
        self.completed.lock().await.get(&id).cloned()
    }

    pub async fn is_interrupted(&self, id: u32) -> bool {
        self.interrupted.lock().await.contains(&id)
    }

    pub async fn clear_completed(&self) {
        self.completed.lock().await.clear();
    }

    pub async fn discard_exec(&self, id: u32) {
        self.execs.lock().await.remove(&id);
    }

    pub async fn discard_interaction(&self, id: u32) {
        self.interactions.lock().await.remove(&id);
    }

    pub async fn drain_running(&self) -> Vec<u32> {
        // Release `execs` before taking the other locks: `reserve_shell_poll` holds
        // `shell_awaits` and then waits for `execs`.
        let mut ids = {
            let mut entries = self.execs.lock().await;
            entries.drain().map(|(id, _)| id).collect::<Vec<_>>()
        };
        ids.sort_unstable();
        self.interactions.lock().await.clear();
        self.shell_awaits.lock().await.clear();
        self.completed.lock().await.clear();
        self.interrupted.lock().await.clear();
        ids
    }

    pub async fn interrupt_for_run_replacement(&self) -> Vec<u32> {
        let mut execs = self.execs.lock().await;
        let mut abort_ids = execs.keys().copied().collect::<Vec<_>>();
        let mut interrupted_ids = abort_ids.clone();
        execs.clear();
        drop(execs);

        let mut interactions = self.interactions.lock().await;
        interrupted_ids.extend(interactions.keys().copied());
        interactions.clear();
        drop(interactions);

        self.shell_awaits.lock().await.clear();
        self.completed.lock().await.clear();
        self.interrupted.lock().await.extend(interrupted_ids);
        abort_ids.sort_unstable();
        abort_ids
    }

    pub async fn interrupt_for_message(&self) -> Vec<u32> {
        let (abort_ids, interrupted_ids) = {
            let mut entries = self.execs.lock().await;
            let mut abort_ids = Vec::new();
            let mut interrupted_ids = Vec::new();
            entries.retain(|id, entry| {
                let keep_running = entry.call.name.eq_ignore_ascii_case("Task");
                if !keep_running {
                    interrupted_ids.push(*id);
                    abort_ids.push(*id);
                }
                keep_running
            });
            (abort_ids, interrupted_ids)
        };
        let interaction_ids = {
            let mut interactions = self.interactions.lock().await;
            let ids = interactions.keys().copied().collect::<Vec<_>>();
            interactions.clear();
            ids
        };
        self.shell_awaits.lock().await.clear();
        let mut interrupted = self.interrupted.lock().await;
        interrupted.extend(interrupted_ids);
        interrupted.extend(interaction_ids);
        let mut abort_ids = abort_ids;
        abort_ids.sort_unstable();
        abort_ids
    }

    pub async fn running_exec_ids(&self) -> Vec<u32> {
        let mut ids = self.execs.lock().await.keys().copied().collect::<Vec<_>>();
        ids.sort_unstable();
        ids
    }

    pub async fn running_task_exec_id(&self, call_id: &str) -> Option<u32> {
        self.execs
            .lock()
            .await
            .iter()
            .filter_map(|(id, entry)| {
                (entry.call.call_id == call_id && entry.call.name.eq_ignore_ascii_case("Task"))
                    .then_some(*id)
            })
            .min()
    }

    pub(crate) async fn task_execs(&self) -> Vec<(u32, ToolCall)> {
        self.execs
            .lock()
            .await
            .iter()
            .filter(|(_, entry)| entry.call.name.eq_ignore_ascii_case("Task"))
            .map(|(id, entry)| (*id, entry.call.clone()))
            .collect()
    }

    fn next_id(&self) -> Result<u32> {
        self.next_id
            .fetch_add(1, Ordering::Relaxed)
            .checked_add(1)
            .ok_or_else(|| Error::Protocol("Cursor message id space exhausted".into()))
    }
}

pub(crate) fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(arguments: serde_json::Value) -> ToolCall {
        ToolCall {
            index: 0,
            call_id: "task-1".into(),
            model_call_id: "model-call-1".into(),
            name: "Task".into(),
            arguments_text: arguments.to_string(),
            arguments,
            argument_error: None,
        }
    }

    fn tool_call(call_id: &str, name: &str) -> ToolCall {
        ToolCall {
            index: 0,
            call_id: call_id.into(),
            model_call_id: "model-call".into(),
            name: name.into(),
            arguments_text: "{}".into(),
            arguments: serde_json::json!({}),
            argument_error: None,
        }
    }

    fn directory(
        hash: &str,
        display_name: &str,
        context_options: &[&str],
        effort_options: &[&str],
    ) -> ModelDirectory {
        ModelDirectory {
            aliases: HashMap::from([
                (display_name.to_string(), hash.to_string()),
                (display_name.to_ascii_lowercase(), hash.to_string()),
            ]),
            variants: HashMap::from([(
                hash.to_string(),
                ModelVariantAxis {
                    context_options: context_options
                        .iter()
                        .map(|value| (*value).into())
                        .collect(),
                    effort_options: effort_options.iter().map(|value| (*value).into()).collect(),
                },
            )]),
            display_names: HashMap::from([(hash.to_string(), display_name.to_string())]),
        }
    }

    #[test]
    fn await_targets_and_timeouts_share_one_strict_contract() {
        use serde_json::json;
        let mut call = tool_call("await", "Await");
        for arguments in [
            json!({}),
            json!({"shell_id": "s", "task_id": "t"}),
            json!({"shell_id": "s", "agentId": "t"}),
            json!({"agent_id": "t"}),
            json!({"task_id": " "}),
            json!({"task_id": "t", "timeout_ms": 3}),
            json!({"task_id": "t", "pattern": "done"}),
            json!({"task_id": "t", "block_until_ms": -1}),
            json!({"task_id": "t", "block_until_ms": 1.5}),
            json!({"task_id": "t", "block_until_ms": 4294967296_u64}),
            json!({"task_id": "t", "block_until_ms": null}),
        ] {
            call.arguments = arguments;
            assert!(await_arguments(&call).is_err(), "{}", call.arguments);
        }
        for target in ["shell_id", "task_id"] {
            for timeout in [json!(0), json!(30000.0), json!(u32::MAX)] {
                call.arguments = json!({target: "target", "block_until_ms": timeout});
                let args = await_arguments(&call).unwrap();
                assert_eq!(args.task_id, "target");
                assert_eq!(
                    f64::from(args.block_until_ms.unwrap()),
                    timeout.as_f64().unwrap()
                );
                assert!(args.regex.is_none());
            }
            call.arguments = json!({target: "target"});
            assert_eq!(await_arguments(&call).unwrap().block_until_ms, Some(30000));
        }
    }

    #[test]
    fn inherited_task_keeps_the_parent_model_variant() {
        let context = ExecContext {
            default_subagent_model: "deepseek-hash".into(),
            default_subagent_model_variant: Some("deepseek-hash-1m-max".into()),
            model_directory: directory("deepseek-hash", "DeepSeek", &["1m"], &["high", "max"]),
            ..ExecContext::default()
        };
        let call = task(serde_json::json!({
            "prompt": "inspect",
            "model": "inherit"
        }));

        let prepared = context
            .prepare_call_with_resume_variant(&call, None)
            .unwrap();
        assert_eq!(prepared.arguments["model"], "deepseek-hash-1m-max");
        assert_eq!(prepared.arguments["model_display"], "DeepSeek");
    }

    #[test]
    fn inherit_is_matched_case_insensitively() {
        let context = ExecContext {
            default_subagent_model: "deepseek-hash".into(),
            default_subagent_model_variant: Some("deepseek-hash-1m-max".into()),
            model_directory: directory("deepseek-hash", "DeepSeek", &["1m"], &["high", "max"]),
            ..ExecContext::default()
        };
        let call = task(serde_json::json!({
            "prompt": "inspect",
            "model": "Inherit"
        }));

        assert_eq!(
            context
                .prepare_call_with_resume_variant(&call, None)
                .unwrap()
                .arguments["model"],
            "deepseek-hash-1m-max"
        );
    }

    #[test]
    fn inherited_task_bakes_explicit_model_parameters_over_the_parent_variant() {
        let context = ExecContext {
            default_subagent_model: "deepseek-hash".into(),
            default_subagent_model_variant: Some("deepseek-hash-1m-max".into()),
            model_directory: directory(
                "deepseek-hash",
                "DeepSeek",
                &["200k", "1m"],
                &["high", "max"],
            ),
            ..ExecContext::default()
        };
        let call = task(serde_json::json!({
            "prompt": "inspect",
            "model_parameters": [{"id": "reasoning", "value": "high"}]
        }));

        // 父变体提供 context 默认值,LLM 的 reasoning 覆盖 effort 分量。
        assert_eq!(
            context
                .prepare_call_with_resume_variant(&call, None)
                .unwrap()
                .arguments["model"],
            "deepseek-hash-1m-high"
        );
    }

    #[test]
    fn model_parameter_values_are_validated_against_the_model_axis() {
        let context = ExecContext {
            default_subagent_model: "hash-deepseek".into(),
            model_directory: directory(
                "hash-deepseek",
                "DeepSeek Flash",
                &["200k", "1m"],
                &["low", "high"],
            ),
            ..ExecContext::default()
        };
        let invalid = task(serde_json::json!({
            "prompt": "inspect",
            "model_parameters": [{"id": "reasoning", "value": "gone"}]
        }));
        let error = context
            .prepare_call_with_resume_variant(&invalid, None)
            .unwrap_err();
        assert!(
            matches!(&error, Error::Protocol(message) if message.contains("low, high")),
            "unexpected error: {error}"
        );

        // 值归一小写后再校验与烘焙,"HIGH" 不会原样发给供应商。
        let upper = task(serde_json::json!({
            "prompt": "inspect",
            "model_parameters": [{"id": "reasoning", "value": "HIGH"}]
        }));
        assert_eq!(
            context
                .prepare_call_with_resume_variant(&upper, None)
                .unwrap()
                .arguments["model"],
            "hash-deepseek-200k-high"
        );
    }

    #[test]
    fn model_parameters_reject_duplicate_ids_and_unknown_ids() {
        let duplicate = serde_json::json!({
            "model_parameters": [
                {"id": "reasoning", "value": "low"},
                {"id": "reasoning", "value": "high"}
            ]
        });
        assert!(matches!(
            parse_task_model_parameters(duplicate.as_object().unwrap()),
            Err(Error::Protocol(message)) if message.contains("repeats reasoning")
        ));
        let unknown = serde_json::json!({
            "model_parameters": [{"id": "effort", "value": "low"}]
        });
        assert!(matches!(
            parse_task_model_parameters(unknown.as_object().unwrap()),
            Err(Error::Protocol(message)) if message.contains("reasoning or context")
        ));
        let wrong_shape = serde_json::json!({"model_parameters": "low"});
        assert!(parse_task_model_parameters(wrong_shape.as_object().unwrap()).is_err());
    }

    #[test]
    fn model_without_a_reasoning_axis_bakes_slugs_without_an_effort_segment() {
        let context = ExecContext {
            default_subagent_model: "plugin-hash".into(),
            model_directory: directory("plugin-hash", "Plugin Model", &["200k", "1m"], &[]),
            ..ExecContext::default()
        };
        let call = task(serde_json::json!({
            "prompt": "inspect",
            "model_parameters": [{"id": "context", "value": "1m"}]
        }));
        assert_eq!(
            context
                .prepare_call_with_resume_variant(&call, None)
                .unwrap()
                .arguments["model"],
            "plugin-hash-1m"
        );
        let reasoning = task(serde_json::json!({
            "prompt": "inspect",
            "model_parameters": [{"id": "reasoning", "value": "high"}]
        }));
        assert!(matches!(
            context.prepare_call_with_resume_variant(&reasoning, None),
            Err(Error::Protocol(message)) if message.contains("not supported")
        ));
    }

    #[test]
    fn model_aliases_canonicalize_display_names_and_slugs() {
        let context = ExecContext {
            default_subagent_model: "parent-model".into(),
            model_directory: directory(
                "hash-deepseek",
                "DeepSeek Flash",
                &["1m"],
                &["low", "high"],
            ),
            ..ExecContext::default()
        };
        // 展示名别名归一到 base hash;不带参数时不烘焙变体。
        let call = task(serde_json::json!({"prompt":"inspect", "model": "DeepSeek Flash"}));
        assert_eq!(
            context
                .prepare_call_with_resume_variant(&call, None)
                .unwrap()
                .arguments["model"],
            "hash-deepseek"
        );
        // 显式变体 slug 保留变体分量,不再折叠到 base hash。
        let variant =
            task(serde_json::json!({"prompt":"inspect", "model": "hash-deepseek-1m-low"}));
        assert_eq!(
            context
                .prepare_call_with_resume_variant(&variant, None)
                .unwrap()
                .arguments["model"],
            "hash-deepseek-1m-low"
        );
        let parameterized = task(serde_json::json!({
            "prompt":"inspect",
            "model":"DeepSeek Flash",
            "model_parameters":[{"id":"reasoning","value":"low"}]
        }));
        assert_eq!(
            context
                .prepare_call_with_resume_variant(&parameterized, None)
                .unwrap()
                .arguments["model"],
            "hash-deepseek-1m-low"
        );
    }

    #[test]
    fn task_model_defaults_to_parent_and_honors_an_explicit_model() {
        let context = ExecContext {
            default_subagent_model: "parent-model".into(),
            model_directory: ModelDirectory {
                display_names: HashMap::from([("child-model".into(), "Child Model".into())]),
                ..Default::default()
            },
            ..ExecContext::default()
        };
        let inherited = context
            .prepare_call_with_resume_variant(&task(serde_json::json!({"prompt":"inspect"})), None)
            .unwrap();
        let explicit = context
            .prepare_call_with_resume_variant(
                &task(serde_json::json!({
                    "prompt":"inspect",
                    "model":"child-model"
                })),
                None,
            )
            .unwrap();

        assert_eq!(inherited.arguments["model"], "parent-model");
        assert_eq!(explicit.arguments["model"], "child-model");
    }

    #[test]
    fn subagent_models_are_selected_by_task_type() {
        let context = ExecContext {
            default_subagent_model: "parent-model".into(),
            subagent_models: HashMap::from([
                (
                    SubagentKind::Named("explore".into()),
                    SubagentModel::Model("explore-model".into()),
                ),
                (SubagentKind::Named("shell".into()), SubagentModel::Inherit),
            ]),
            ..ExecContext::default()
        };
        let explore = task(serde_json::json!({
            "prompt":"inspect",
            "subagent_type":"explore",
            "model":"gpt-5.6-sol"
        }));
        let shell = task(serde_json::json!({
            "prompt":"inspect",
            "subagent_type":"shell",
            "model":"k3-256k"
        }));

        assert_eq!(
            context
                .prepare_call_with_resume_variant(&explore, None)
                .unwrap()
                .arguments["model"],
            "explore-model"
        );
        assert_eq!(
            context
                .prepare_call_with_resume_variant(&shell, None)
                .unwrap()
                .arguments["model"],
            "parent-model"
        );
    }

    #[test]
    fn disabled_subagent_type_only_disables_that_type() {
        let context = ExecContext {
            default_subagent_model: "parent-model".into(),
            subagent_models: HashMap::from([(
                SubagentKind::Named("shell".into()),
                SubagentModel::Disabled,
            )]),
            ..ExecContext::default()
        };
        let shell = task(serde_json::json!({
            "prompt":"inspect",
            "subagent_type":"shell"
        }));
        let explore = task(serde_json::json!({
            "prompt":"inspect",
            "subagent_type":"explore"
        }));

        assert!(context.task_disabled(&shell));
        assert!(!context.task_disabled(&explore));
        assert!(context
            .prepare_call_with_resume_variant(&shell, None)
            .unwrap()
            .arguments
            .get("model")
            .is_none());
        assert_eq!(
            context
                .prepare_call_with_resume_variant(&explore, None)
                .unwrap()
                .arguments["model"],
            "parent-model"
        );
    }

    #[tokio::test]
    async fn a_message_interrupt_does_not_mute_the_tasks_it_keeps_running() {
        let runtime = CursorToolRuntime::default();
        let context = ExecContext::default();
        let task = runtime
            .reserve_exec(&tool_call("call-task", "Task"), &context)
            .await
            .unwrap();
        let shell = runtime
            .reserve_exec(&tool_call("call-shell", "Shell"), &context)
            .await
            .unwrap();

        assert_eq!(runtime.interrupt_for_message().await, vec![shell]);

        // The Task was deliberately left running, so its events must still land.
        assert!(!runtime.is_interrupted(task).await);
        assert!(runtime.append_stdout(task, "still running").await);
        assert!(runtime.is_interrupted(shell).await);
    }

    #[tokio::test]
    async fn a_message_interrupt_still_mutes_and_aborts_everything_else() {
        let runtime = CursorToolRuntime::default();
        let context = ExecContext::default();
        let read = runtime
            .reserve_exec(&tool_call("call-read", "Read"), &context)
            .await
            .unwrap();
        let interaction = runtime
            .reserve_interaction(&tool_call("call-ask", "AskQuestion"))
            .await
            .unwrap();

        assert_eq!(runtime.interrupt_for_message().await, vec![read]);

        assert!(runtime.is_interrupted(read).await);
        assert!(runtime.is_interrupted(interaction).await);
    }
}
