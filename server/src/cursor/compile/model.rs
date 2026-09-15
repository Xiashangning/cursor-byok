//! Resolves Cursor model selections to configured provider models.
use crate::{
    cursor::protocol::proto::agent::v1 as pb,
    model::{
        parse_token_count, ModelLatency, ModelSpec, ReasoningSpec, SubagentKind,
        SubagentModelOverride,
    },
    Error, Result,
};

pub fn requested_model(request: &pb::AgentRunRequest) -> Result<ModelSpec> {
    let details = request.model_details.as_ref();
    let model = if let Some(requested) = request.requested_model.as_ref() {
        from_requested(requested, details)?
    } else if let Some(model_id) = details
        .map(|model| model.model_id.as_str())
        .filter(|model| !model.is_empty())
    {
        ModelSpec {
            model_id: model_id.into(),
            display_name: details
                .map(|model| model.display_name.clone())
                .filter(|name| !name.is_empty()),
            reasoning: ReasoningSpec {
                enabled: details.is_some_and(|model| model.thinking_details.is_some()),
                effort: None,
                explicitly_disabled: false,
            },
            latency: ModelLatency::Standard,
            max_output_tokens: None,
            context_window_tokens: None,
            supports_image_generation: false,
            extra_params: serde_json::json!({}),
        }
    } else {
        return Err(Error::Protocol("Cursor Run does not select a model".into()));
    };
    Ok(model)
}

pub fn overrides(
    request: &pb::AgentRunRequest,
) -> Result<Vec<(SubagentKind, SubagentModelOverride)>> {
    request
        .subagent_model_overrides
        .iter()
        .map(|value| {
            use pb::subagent_model_override::Selection;
            let kind = subagent_kind(&value.subagent_type);
            let selection = match value.selection.as_ref() {
                Some(Selection::Model(model)) => {
                    if model.model_id == "default" {
                        SubagentModelOverride::Inherit
                    } else {
                        SubagentModelOverride::Explicit(from_requested(model, None)?)
                    }
                }
                Some(Selection::Inherit(true)) => SubagentModelOverride::Inherit,
                Some(Selection::Disabled(true)) => SubagentModelOverride::Disabled,
                None | Some(Selection::Inherit(false) | Selection::Disabled(false)) => {
                    return Err(Error::Protocol(format!(
                        "Cursor subagent model override {} has no active selection",
                        value.subagent_type
                    )))
                }
            };
            Ok((kind, selection))
        })
        .collect()
}

pub fn subagent_kind(value: &str) -> SubagentKind {
    if value == "generalPurpose" {
        SubagentKind::GeneralPurpose
    } else {
        SubagentKind::Named(value.into())
    }
}

fn from_requested(
    model: &pb::RequestedModel,
    details: Option<&pb::ModelDetails>,
) -> Result<ModelSpec> {
    let mut spec = ModelSpec {
        model_id: model.model_id.clone(),
        display_name: details
            .map(|model| model.display_name.clone())
            .filter(|name| !name.is_empty()),
        reasoning: ReasoningSpec {
            enabled: model.max_mode
                || details.is_some_and(|model| model.thinking_details.is_some()),
            effort: None,
            explicitly_disabled: false,
        },
        latency: ModelLatency::Standard,
        max_output_tokens: None,
        context_window_tokens: None,
        supports_image_generation: false,
        extra_params: serde_json::json!({}),
    };
    apply_requested_parameters(&mut spec, model)?;
    Ok(spec)
}

pub(super) fn apply_requested_parameters(
    spec: &mut ModelSpec,
    model: &pb::RequestedModel,
) -> Result<()> {
    for parameter in &model.parameters {
        // 空值是 Cursor 回程携带的未赋值占位,视为未设置;
        // 不能把占位当成显式的"关闭",否则会抹掉变体 slug 与配置默认的 effort。
        let value = parameter.value.trim();
        if value.is_empty() {
            continue;
        }
        match parameter.id.as_str() {
            "effort" | "reasoning" => {
                let effort = value.to_ascii_lowercase();
                spec.reasoning.explicitly_disabled = matches!(effort.as_str(), "none" | "off");
                spec.reasoning.effort = (!spec.reasoning.explicitly_disabled).then_some(effort);
                spec.reasoning.enabled = spec.reasoning.effort.is_some();
            }
            "thinking" => {
                spec.reasoning.enabled = parse_bool(parameter)?;
                spec.reasoning.explicitly_disabled = !spec.reasoning.enabled;
                if spec.reasoning.explicitly_disabled {
                    spec.reasoning.effort = None;
                }
            }
            "fast" => {
                spec.latency = if parse_bool(parameter)? {
                    ModelLatency::Fast
                } else {
                    ModelLatency::Standard
                };
            }
            "context" => {
                spec.context_window_tokens =
                    Some(parse_token_count(&parameter.value).ok_or_else(|| {
                        Error::Protocol(format!(
                            "invalid Cursor context token count: {}",
                            parameter.value
                        ))
                    })?);
            }
            _ => {}
        }
    }
    Ok(())
}

fn parse_bool(parameter: &pb::requested_model::ModelParameterValue) -> Result<bool> {
    match parameter.value.as_str() {
        "true" => Ok(true),
        "false" => Ok(false),
        _ => Err(Error::Protocol(format!(
            "invalid Cursor boolean model parameter {}={}",
            parameter.id, parameter.value
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ignores_unknown_cursor_model_parameters() {
        let requested = pb::RequestedModel {
            model_id: "test-model".into(),
            parameters: vec![pb::requested_model::ModelParameterValue {
                id: "optimize_for".into(),
                value: "quality".into(),
            }],
            ..Default::default()
        };

        let model = from_requested(&requested, None).expect("unknown parameter should be ignored");

        assert_eq!(model.model_id, "test-model");
        assert_eq!(model.latency, ModelLatency::Standard);
        assert!(!model.reasoning.enabled);
    }

    #[test]
    fn empty_parameter_values_are_unset_not_explicit_disables() {
        let requested = pb::RequestedModel {
            model_id: "test-model".into(),
            parameters: ["reasoning", "context", "thinking", "fast"]
                .into_iter()
                .map(|id| pb::requested_model::ModelParameterValue {
                    id: id.into(),
                    value: "  ".into(),
                })
                .collect(),
            ..Default::default()
        };
        let mut spec = ModelSpec::new("test-model");
        spec.reasoning.enabled = true;
        spec.reasoning.effort = Some("high".into());

        apply_requested_parameters(&mut spec, &requested).unwrap();

        assert_eq!(spec.reasoning.effort.as_deref(), Some("high"));
        assert!(spec.reasoning.enabled);
        assert!(spec.context_window_tokens.is_none());
        assert_eq!(spec.latency, ModelLatency::Standard);
    }
}
