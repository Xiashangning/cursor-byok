//! Compiles stable Prompt specifications for Cursor modes.
use std::collections::BTreeMap;

use crate::{
    model::{ModelSpec, PromptSpec, ToolDefinition},
    Error, Result,
};

use super::{assets::runtime_expression, Mode, PromptAssets};

#[derive(Clone)]
pub struct PromptCompiler {
    assets: PromptAssets,
}

impl PromptCompiler {
    pub fn new(assets: PromptAssets) -> Self {
        Self { assets }
    }

    pub fn runtime_message(&self, mode: Mode, values: &BTreeMap<&str, String>) -> Result<String> {
        render(&self.assets.mode(mode).runtime, values)
    }

    /// Whether the mode's tool descriptions reference {{AVAILABLE_SUBAGENT_MODELS}}.
    /// Modes without orchestration tools (subagent, compaction, ...) skip the lookup.
    pub fn needs_available_subagent_models(&self, mode: Mode) -> bool {
        self.assets
            .mode(mode)
            .tools
            .iter()
            .any(|tool| tool.description.contains("{{AVAILABLE_SUBAGENT_MODELS}}"))
    }

    pub fn prompt_spec(
        &self,
        mode: Mode,
        model: &ModelSpec,
        dynamic_tools: &[ToolDefinition],
        suppress_subagent_progress: bool,
    ) -> Result<PromptSpec> {
        self.prompt_spec_with_available_subagent_models(
            mode,
            model,
            dynamic_tools,
            suppress_subagent_progress,
            "",
        )
    }

    pub fn prompt_spec_with_available_subagent_models(
        &self,
        mode: Mode,
        model: &ModelSpec,
        dynamic_tools: &[ToolDefinition],
        suppress_subagent_progress: bool,
        available_subagent_models: &str,
    ) -> Result<PromptSpec> {
        let mut tools = self.tools(mode, suppress_subagent_progress);
        let mut dynamic_tools = dynamic_tools.to_vec();
        dynamic_tools.sort_by(|left, right| left.name.cmp(&right.name));
        append_dynamic_tools(&mut tools, dynamic_tools)?;
        if !model.supports_image_generation {
            tools.retain(|tool| tool.name != "GenerateImage");
        }
        let fake_model_name = model
            .display_name
            .as_deref()
            .unwrap_or(model.model_id.as_str());
        let instructions = self
            .assets
            .mode(mode)
            .prompt
            .replace("{{FAKE_MODEL_NAME}}", fake_model_name);
        for tool in &mut tools {
            tool.description = tool
                .description
                .replace("{{AVAILABLE_SUBAGENT_MODELS}}", available_subagent_models);
        }
        Ok(PromptSpec {
            instructions,
            tools,
        })
    }

    fn tools(&self, mode: Mode, suppress_subagent_progress: bool) -> Vec<ToolDefinition> {
        let mut tools = self.assets.mode(mode).tools.clone();
        if mode == Mode::Subagent && suppress_subagent_progress {
            tools.retain(|tool| tool.name != "UpdateCurrentStep");
        }
        tools
    }
}

fn render(template: &str, values: &BTreeMap<&str, String>) -> Result<String> {
    let expression = runtime_expression();
    let mut output = String::with_capacity(template.len());
    let mut cursor = 0;
    for capture in expression.captures_iter(template) {
        let token = capture.get(0).expect("runtime template token");
        let name = &capture[1];
        let value = values
            .get(name)
            .ok_or_else(|| Error::Protocol(format!("runtime template value is missing: {name}")))?;
        output.push_str(&template[cursor..token.start()]);
        output.push_str(value);
        cursor = token.end();
    }
    output.push_str(&template[cursor..]);
    Ok(output.trim().to_string())
}

fn append_dynamic_tools(
    tools: &mut Vec<ToolDefinition>,
    additions: Vec<ToolDefinition>,
) -> Result<()> {
    for tool in additions {
        if tools.iter().any(|existing| existing.name == tool.name) {
            return Err(Error::Protocol(format!(
                "dynamic MCP tool conflicts with a mode tool: {}",
                tool.name
            )));
        }
        tools.push(tool);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_description_receives_dynamic_model_parameter_options() {
        let compiler = PromptCompiler::new(PromptAssets::embedded().unwrap());
        let prompt = compiler
            .prompt_spec_with_available_subagent_models(
                Mode::Multitask,
                &ModelSpec::new("model"),
                &[],
                false,
                "- inherit\n- Configured [provider-model] — effort: low, high; context: 272k, 1m",
            )
            .unwrap();
        let task = prompt
            .tools
            .iter()
            .find(|tool| tool.name == "Task")
            .unwrap();
        assert!(task.description.contains("Configured [provider-model]"));
        assert!(task
            .description
            .contains("effort: low, high; context: 272k, 1m"));
        assert!(!task.description.contains("{{AVAILABLE_SUBAGENT_MODELS}}"));
    }
}
