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
    model::{SubagentKind, ToolCall},
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
    EditRead,
    EditWrite(EditWrite),
}

#[derive(Clone, Debug, Default)]
pub struct ExecContext {
    pub conversation_id: String,
    pub root_conversation_id: String,
    pub default_subagent_model: String,
    pub default_subagent_model_variant: Option<String>,
    pub model_aliases: HashMap<String, String>,
    pub model_variant_defaults: HashMap<String, (String, String)>,
    pub subagent_models: HashMap<SubagentKind, SubagentModel>,
    pub allow_subagents: bool,
    pub terminals_folder: String,
    pub admin_command_denylist: Vec<String>,
    pub mcp_routes: HashMap<(String, String), McpRoute>,
}

#[derive(Clone, Debug)]
pub struct McpRoute {
    pub name: String,
    pub provider_identifier: String,
    pub tool_name: String,
    pub description: String,
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
        "task" | "create-agent" | "send-message-to-agent"
    )
}

pub(crate) fn subagent_kind(value: &str) -> SubagentKind {
    if value == "generalPurpose" {
        SubagentKind::GeneralPurpose
    } else {
        SubagentKind::Named(value.into())
    }
}

fn task_parameter(
    arguments: &serde_json::Map<String, serde_json::Value>,
    id: &str,
) -> Option<String> {
    let value = arguments.get("model_parameters")?;
    let values = match value {
        serde_json::Value::Array(values) => values.iter().collect::<Vec<_>>(),
        serde_json::Value::Object(_) => vec![value],
        _ => return None,
    };
    values.into_iter().find_map(|value| {
        let object = value.as_object()?;
        (object.get("id").and_then(serde_json::Value::as_str) == Some(id))
            .then(|| object.get("value").and_then(serde_json::Value::as_str))
            .flatten()
            .map(str::to_string)
    })
}

impl ExecContext {
    fn canonical_model(&self, model: &str) -> String {
        self.model_aliases
            .get(model)
            .or_else(|| self.model_aliases.get(&model.to_ascii_lowercase()))
            .cloned()
            .unwrap_or_else(|| model.to_string())
    }

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

    pub fn prepare_call(&self, call: &ToolCall) -> Result<ToolCall> {
        if !call.name.eq_ignore_ascii_case("Task") {
            return Ok(call.clone());
        }
        let arguments = call
            .arguments
            .as_object()
            .ok_or_else(|| Error::Protocol("Task arguments must be a JSON object".into()))?;
        let subagent_type = arguments
            .get("subagent_type")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("generalPurpose");
        if self.task_disabled(call) {
            return Ok(call.clone());
        }
        let override_model = self.subagent_model_for(subagent_type);
        let inherits_default = match override_model {
            Some(SubagentModel::Model(_)) | Some(SubagentModel::Disabled) => false,
            Some(SubagentModel::Inherit) => true,
            None => arguments
                .get("model")
                .and_then(serde_json::Value::as_str)
                .is_none_or(|model| model == "inherit"),
        };
        let model = match override_model {
            Some(SubagentModel::Model(model)) => model.clone(),
            Some(SubagentModel::Inherit) => self
                .default_subagent_model_variant
                .clone()
                .unwrap_or_else(|| self.default_subagent_model.clone()),
            Some(SubagentModel::Disabled) => unreachable!("disabled Task returned above"),
            None => arguments
                .get("model")
                .and_then(serde_json::Value::as_str)
                .filter(|model| *model != "inherit")
                .map_or_else(
                    || {
                        self.default_subagent_model_variant
                            .clone()
                            .unwrap_or_else(|| self.default_subagent_model.clone())
                    },
                    str::to_owned,
                ),
        };
        let model = if inherits_default {
            model
        } else {
            self.canonical_model(&model)
        };
        let model = if let Some((default_context, default_effort)) =
            self.model_variant_defaults.get(&model)
        {
            let has_parameters = arguments.contains_key("model_parameters");
            if has_parameters {
                let context =
                    task_parameter(arguments, "context").unwrap_or_else(|| default_context.clone());
                let effort =
                    task_parameter(arguments, "effort").unwrap_or_else(|| default_effort.clone());
                format!("{model}-{context}-{effort}")
            } else {
                model
            }
        } else {
            model
        };
        if model.is_empty() {
            return Err(Error::Protocol(format!(
                "Task subagent type {subagent_type} has no model"
            )));
        }
        let mut prepared = call.clone();
        prepared
            .arguments
            .as_object_mut()
            .expect("Task arguments were validated")
            .insert("model".into(), serde_json::Value::String(model));
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
        }
    }

    pub async fn reserve_exec(&self, call: &ToolCall, context: &ExecContext) -> Result<u32> {
        self.reserve_exec_stage(call, context, ExecStage::Direct, None)
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
        let mut entries = self.execs.lock().await;
        let mut ids = entries.drain().map(|(id, _)| id).collect::<Vec<_>>();
        ids.sort_unstable();
        self.interactions.lock().await.clear();
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
                interrupted_ids.push(*id);
                let keep_running = entry.call.name.eq_ignore_ascii_case("Task");
                if !keep_running {
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

    #[test]
    fn inherited_task_keeps_the_parent_model_variant() {
        let context = ExecContext {
            default_subagent_model: "deepseek-hash".into(),
            default_subagent_model_variant: Some("deepseek-hash-1m-max".into()),
            model_variant_defaults: HashMap::from([(
                "deepseek-hash".into(),
                ("1m".into(), "high".into()),
            )]),
            ..ExecContext::default()
        };
        let call = task(serde_json::json!({
            "prompt": "inspect",
            "model": "inherit"
        }));

        assert_eq!(
            context.prepare_call(&call).unwrap().arguments["model"],
            "deepseek-hash-1m-max"
        );
    }

    #[test]
    fn model_aliases_canonicalize_display_names_and_slugs() {
        let context = ExecContext {
            default_subagent_model: "parent-model".into(),
            model_aliases: HashMap::from([
                ("DeepSeek Flash".into(), "hash-deepseek".into()),
                ("deepseek flash".into(), "hash-deepseek".into()),
                ("hash-deepseek-1m-low".into(), "hash-deepseek".into()),
            ]),
            model_variant_defaults: HashMap::from([(
                "hash-deepseek".into(),
                ("1m".into(), "high".into()),
            )]),
            ..ExecContext::default()
        };
        for model in ["DeepSeek Flash", "hash-deepseek-1m-low"] {
            let call = task(serde_json::json!({"prompt":"inspect", "model": model}));
            assert_eq!(
                context.prepare_call(&call).unwrap().arguments["model"],
                "hash-deepseek"
            );
        }
        let parameterized = task(serde_json::json!({
            "prompt":"inspect",
            "model":"DeepSeek Flash",
            "model_parameters":[{"id":"effort","value":"low"}]
        }));
        assert_eq!(
            context.prepare_call(&parameterized).unwrap().arguments["model"],
            "hash-deepseek-1m-low"
        );
    }

    #[test]
    fn task_model_defaults_to_parent_and_honors_an_explicit_model() {
        let context = ExecContext {
            default_subagent_model: "parent-model".into(),
            ..ExecContext::default()
        };
        let inherited = context
            .prepare_call(&task(serde_json::json!({"prompt":"inspect"})))
            .unwrap();
        let explicit = context
            .prepare_call(&task(serde_json::json!({
                "prompt":"inspect",
                "model":"child-model"
            })))
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
            context.prepare_call(&explore).unwrap().arguments["model"],
            "explore-model"
        );
        assert_eq!(
            context.prepare_call(&shell).unwrap().arguments["model"],
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
            .prepare_call(&shell)
            .unwrap()
            .arguments
            .get("model")
            .is_none());
        assert_eq!(
            context.prepare_call(&explore).unwrap().arguments["model"],
            "parent-model"
        );
    }
}
