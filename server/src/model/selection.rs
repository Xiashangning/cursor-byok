//! Resolved model identity and independent execution parameters.
use serde::{Deserialize, Serialize};

use super::{ModelDirectory, ModelVariantParts, Resolution};
use crate::{Error, Result};

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SelectionParameters {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fast: Option<bool>,
}

impl From<ModelVariantParts> for SelectionParameters {
    fn from(parts: ModelVariantParts) -> Self {
        Self {
            context: Some(parts.context),
            reasoning: parts.effort,
            fast: Some(parts.fast),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ModelSelection {
    Local {
        id: String,
        #[serde(default)]
        parameters: SelectionParameters,
    },
    Official {
        model: String,
        #[serde(default)]
        parameters: SelectionParameters,
    },
}

impl ModelSelection {
    pub fn resolve(directory: &ModelDirectory, input: &str) -> Result<Self> {
        match directory.resolve(input) {
            Resolution::Matched { id, parts } => Ok(Self::matched(directory, id, parts)),
            Resolution::NotFound => Ok(Self::Official {
                model: input.into(),
                parameters: SelectionParameters::default(),
            }),
            Resolution::Ambiguous { candidates } => Err(Error::Config(format!(
                "model '{input}' is ambiguous; use one of the model IDs: {}",
                candidates.join(", ")
            ))),
        }
    }

    /// 已命中条目的选择构造:无变体分量时回填目录默认变体。
    pub fn matched(
        directory: &ModelDirectory,
        id: String,
        parts: Option<ModelVariantParts>,
    ) -> Self {
        let parameters = parts
            .or_else(|| directory.default_parts(&id))
            .map(Into::into)
            .unwrap_or_default();
        Self::Local { id, parameters }
    }

    pub fn id(&self) -> &str {
        match self {
            Self::Local { id, .. } => id,
            Self::Official { model, .. } => model,
        }
    }

    pub fn parameters(&self) -> &SelectionParameters {
        match self {
            Self::Local { parameters, .. } | Self::Official { parameters, .. } => parameters,
        }
    }

    pub fn parameters_mut(&mut self) -> &mut SelectionParameters {
        match self {
            Self::Local { parameters, .. } | Self::Official { parameters, .. } => parameters,
        }
    }

    /// Slugs exist only on the Cursor wire, never in persisted selection state.
    /// 编码必须能被同一目录的 parse_slug 回读:effort 段只在 effort 轴非空时
    /// 存在;轴非空但缺少 reasoning 时无法烘焙合法 slug,退回裸 ID
    /// (解析方按默认变体处理),而不是产出会被解析拒绝的部分 slug。
    pub fn cursor_model_id(&self, directory: &ModelDirectory) -> String {
        let parameters = self.parameters();
        let Self::Local { id, .. } = self else {
            return self.id().into();
        };
        let Some(context) = &parameters.context else {
            return id.clone();
        };
        let Some(entry) = directory.get(id) else {
            return id.clone();
        };
        if !entry.axis.effort_options.is_empty() && parameters.reasoning.is_none() {
            return id.clone();
        }
        entry.axis.bake_slug(
            id,
            &ModelVariantParts {
                context: context.clone(),
                effort: parameters.reasoning.clone(),
                fast: parameters.fast == Some(true),
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ModelConfig;

    fn directory(effort_options: &[&str]) -> ModelDirectory {
        let model = ModelConfig {
            model_hash: "abcd1234".into(),
            display_name: "Model".into(),
            model_id: "provider-model".into(),
            effort_options: effort_options.iter().map(|value| (*value).into()).collect(),
            context_options: vec!["200k".into(), "1m".into()],
            ..Default::default()
        };
        ModelDirectory::new(std::slice::from_ref(&model), &[]).unwrap()
    }

    fn local(parameters: SelectionParameters) -> ModelSelection {
        ModelSelection::Local {
            id: "abcd1234".into(),
            parameters,
        }
    }

    #[test]
    fn structured_selection_round_trip() {
        for value in [
            serde_json::json!({"kind":"local","id":"abcd1234","parameters":{"context":"200k","reasoning":"high","fast":false}}),
            serde_json::json!({"kind":"official","model":"unknown-model","parameters":{}}),
        ] {
            let selection: ModelSelection = serde_json::from_value(value.clone()).unwrap();
            assert_eq!(serde_json::to_value(selection).unwrap(), value);
        }
    }

    #[test]
    fn cursor_model_id_round_trips_through_the_directory_parser() {
        let directory = directory(&["low", "high"]);
        let selection = local(SelectionParameters {
            context: Some("1m".into()),
            reasoning: Some("low".into()),
            fast: Some(true),
        });
        let slug = selection.cursor_model_id(&directory);
        assert_eq!(slug, "abcd1234-1m-low-fast");
        assert_eq!(
            directory.resolve(&slug),
            Resolution::Matched {
                id: "abcd1234".into(),
                parts: Some(ModelVariantParts {
                    context: "1m".into(),
                    effort: Some("low".into()),
                    fast: true,
                })
            }
        );
    }

    #[test]
    fn cursor_model_id_never_bakes_a_slug_its_own_parser_rejects() {
        // 无 effort 轴:reasoning 分量不进入 slug(空轴解析器会把 effort 段
        // 并进 context 再拒绝)。
        let no_effort = directory(&[]);
        let selection = local(SelectionParameters {
            context: Some("1m".into()),
            reasoning: Some("low".into()),
            fast: Some(true),
        });
        let slug = selection.cursor_model_id(&no_effort);
        assert_eq!(slug, "abcd1234-1m-fast");
        assert!(matches!(
            no_effort.resolve(&slug),
            Resolution::Matched { .. }
        ));

        // 有 effort 轴但缺 reasoning:无法烘焙合法 slug,退到裸 ID,
        // 由解析方按默认变体处理。
        let with_effort = directory(&["low", "high"]);
        let selection = local(SelectionParameters {
            context: Some("1m".into()),
            reasoning: None,
            fast: Some(true),
        });
        assert_eq!(selection.cursor_model_id(&with_effort), "abcd1234");
    }
}
