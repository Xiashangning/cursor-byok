//! Model identity lookup, name resolution, and cross-source collision validation.
use std::collections::{BTreeMap, HashMap};

use crate::{model::ModelConfig, plugin::PluginModelDescriptor, Error, Result};

use super::{ModelVariantAxis, ModelVariantParts};

/// 目录中一个模型的来源。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ModelSource {
    /// 内置配置模型。
    Builtin,
    /// 插件模型(插件/供应商引用保留在描述符字段中)。
    Plugin,
}

/// 目录条目:统一 ID + 展示名 + 上游模型名 + 来源 + 参数轴。
#[derive(Clone, Debug)]
pub struct DirectoryEntry {
    pub id: String,
    pub display_name: String,
    /// 供应商实际使用的上游模型名(内置的 model_id 或插件 model_id)。
    pub upstream_model_id: String,
    pub source: ModelSource,
    pub axis: ModelVariantAxis,
    /// 插件引用(仅插件来源);内置模型为 None。
    pub plugin: Option<PluginModelDescriptor>,
}

impl DirectoryEntry {
    /// 缺上游模型名的内置配置是可编辑草稿;执行与目录发布都拒绝草稿。
    pub fn is_draft(&self) -> bool {
        self.source == ModelSource::Builtin && self.upstream_model_id.is_empty()
    }
}

/// 唯一匹配函数的结果。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Resolution {
    /// 唯一命中:统一 ID + 变体分量(输入是变体 slug 时携带)。
    Matched {
        id: String,
        parts: Option<ModelVariantParts>,
    },
    /// 名称命中多个模型:返回全部候选 ID,禁止按遍历顺序任选。
    Ambiguous { candidates: Vec<String> },
    /// 目录中没有任何模型命中。
    NotFound,
}

/// 合并目录。构建时检测 ID 冲突,冲突返回明确错误。
/// `Default` 是空目录(无模型),运行时目录由 `new` 构建。
#[derive(Clone, Debug, Default)]
pub struct ModelDirectory {
    entries: BTreeMap<String, DirectoryEntry>,
    /// 名称索引:NOCASE 匹配名(展示名/上游模型名)→ 候选 ID 集合。
    name_index: HashMap<String, Vec<String>>,
}

impl ModelDirectory {
    pub fn new(models: &[ModelConfig], plugin_models: &[PluginModelDescriptor]) -> Result<Self> {
        let mut entries = BTreeMap::new();
        let mut name_index: HashMap<String, Vec<String>> = HashMap::new();
        for model in models {
            let id = model.model_hash.clone();
            if let Some(existing) = entries.get(&id) {
                return Err(conflict(&id, existing, &describe_builtin(model)));
            }
            let axis = model.variant_axis();
            index_name(&mut name_index, &id, &model.display_name);
            index_name(&mut name_index, &id, &model.model_id);
            entries.insert(
                id,
                DirectoryEntry {
                    id: model.model_hash.clone(),
                    display_name: model.display_name.clone(),
                    upstream_model_id: model.model_id.clone(),
                    source: ModelSource::Builtin,
                    axis,
                    plugin: None,
                },
            );
        }
        for descriptor in plugin_models {
            let id = descriptor.id.clone();
            if let Some(existing) = entries.get(&id) {
                return Err(conflict(&id, existing, &describe_plugin(descriptor)));
            }
            let axis = descriptor.variant_axis();
            index_name(&mut name_index, &id, &descriptor.display_name);
            index_name(&mut name_index, &id, &descriptor.model_id);
            entries.insert(
                id,
                DirectoryEntry {
                    id: descriptor.id.clone(),
                    display_name: descriptor.display_name.clone(),
                    upstream_model_id: descriptor.model_id.clone(),
                    source: ModelSource::Plugin,
                    axis,
                    plugin: Some(descriptor.clone()),
                },
            );
        }
        for candidates in name_index.values_mut() {
            candidates.sort();
        }
        Ok(Self {
            entries,
            name_index,
        })
    }

    pub fn get(&self, id: &str) -> Option<&DirectoryEntry> {
        self.entries.get(id)
    }

    pub fn entries(&self) -> impl Iterator<Item = &DirectoryEntry> {
        self.entries.values()
    }

    /// 运行期目录:全部内置配置 + 已配置插件模型(禁用与未配置的插件模型不在其内)。
    pub async fn configured(
        store: &crate::store::Store,
        plugins: Option<&crate::plugin::PluginRegistry>,
    ) -> Result<Self> {
        let plugin_models = match plugins {
            Some(plugins) => plugins.configured_models().await,
            None => Vec::new(),
        };
        Self::new(&store.models().await?, &plugin_models)
    }

    /// 唯一匹配:精确 ID(区分大小写)→ 变体 slug → 名称(NOCASE)。
    /// 歧义必须返回候选,禁止按遍历顺序取第一个。
    pub fn resolve(&self, key: &str) -> Resolution {
        if let Some(entry) = self.entries.get(key) {
            return Resolution::Matched {
                id: entry.id.clone(),
                parts: None,
            };
        }
        let lowercase = key.to_ascii_lowercase();
        for entry in self.entries.values() {
            if let Some(parts) = entry
                .axis
                .parse_slug(&entry.id, key)
                .or_else(|| entry.axis.parse_slug(&entry.id, &lowercase))
            {
                return Resolution::Matched {
                    id: entry.id.clone(),
                    parts: Some(parts),
                };
            }
        }
        let candidates = self
            .name_index
            .get(&lowercase)
            .map(Vec::as_slice)
            .unwrap_or_default();
        match candidates {
            [] => Resolution::NotFound,
            [id] => Resolution::Matched {
                id: id.clone(),
                parts: None,
            },
            _ => Resolution::Ambiguous {
                candidates: candidates.to_vec(),
            },
        }
    }

    /// 目录中是否含有该 ID(仅精确匹配)。
    pub fn contains(&self, id: &str) -> bool {
        self.entries.contains_key(id)
    }

    /// 为无变体分量输入解析默认变体;无 context 轴时返回 None。
    pub fn default_parts(&self, id: &str) -> Option<ModelVariantParts> {
        self.entries
            .get(id)
            .and_then(|entry| entry.axis.default_parts())
    }
}

fn index_name(index: &mut HashMap<String, Vec<String>>, id: &str, name: &str) {
    let key = name.to_ascii_lowercase();
    if key.is_empty() {
        return;
    }
    let entry = index.entry(key).or_default();
    if !entry.iter().any(|candidate| candidate == id) {
        entry.push(id.to_string());
    }
}

fn conflict(id: &str, existing: &DirectoryEntry, other: &str) -> Error {
    Error::Config(format!(
        "model ID conflict: '{id}' maps to both {} and {other}; refusing to pick one silently",
        describe_entry(existing),
    ))
}

fn describe_entry(entry: &DirectoryEntry) -> String {
    match entry.source {
        ModelSource::Builtin => format!("built-in model '{}'", entry.display_name),
        ModelSource::Plugin => format!(
            "plugin model '{}'",
            entry
                .plugin
                .as_ref()
                .map(|plugin| plugin.display_name.as_str())
                .unwrap_or_default()
        ),
    }
}

fn describe_builtin(model: &ModelConfig) -> String {
    format!("built-in model '{}'", model.display_name)
}

fn describe_plugin(descriptor: &PluginModelDescriptor) -> String {
    format!(
        "plugin model '{}' from plugin '{}'",
        descriptor.display_name, descriptor.plugin_name
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ModelConfig;

    fn builtin(id: &str, display_name: &str, model_id: &str) -> ModelConfig {
        ModelConfig {
            model_hash: id.into(),
            display_name: display_name.into(),
            model_id: model_id.into(),
            effort_options: vec!["low".into(), "high".into()],
            context_options: vec!["200k".into(), "1m".into()],
            ..Default::default()
        }
    }

    fn plugin(id: &str, display_name: &str, model_id: &str) -> PluginModelDescriptor {
        PluginModelDescriptor {
            id: id.into(),
            plugin_id: "dev.example".into(),
            plugin_name: "Example".into(),
            provider_id: "codex".into(),
            model_id: model_id.into(),
            display_name: display_name.into(),
            provider_type: "openai".into(),
            enabled: true,
            effort_options: vec!["low".into(), "high".into()],
            context_options: vec!["200k".into(), "1m".into()],
            ..Default::default()
        }
    }

    fn directory() -> ModelDirectory {
        ModelDirectory::new(
            &[builtin("11112222", "DeepSeek", "deepseek-chat")],
            &[plugin("33334444", "GPT-5", "org/gpt-5")],
        )
        .unwrap()
    }

    #[test]
    fn exact_ids_resolve_across_both_sources() {
        let directory = directory();
        assert_eq!(
            directory.resolve("11112222"),
            Resolution::Matched {
                id: "11112222".into(),
                parts: None
            }
        );
        assert_eq!(
            directory.resolve("33334444"),
            Resolution::Matched {
                id: "33334444".into(),
                parts: None
            }
        );
        assert_eq!(directory.resolve("11112223"), Resolution::NotFound);
    }

    #[test]
    fn names_match_case_insensitively_and_slugs_carry_parts() {
        let directory = directory();
        for key in ["DeepSeek", "deepseek", "GPT-5", "org/gpt-5"] {
            assert!(
                matches!(directory.resolve(key), Resolution::Matched { .. }),
                "key {key} must resolve"
            );
        }
        let Resolution::Matched { id, parts } = directory.resolve("11112222-1m-low") else {
            panic!("variant slug must resolve")
        };
        assert_eq!(id, "11112222");
        let parts = parts.unwrap();
        assert_eq!(parts.context, "1m");
        assert_eq!(parts.effort.as_deref(), Some("low"));
        assert_eq!(
            directory.resolve("11112222-1M-LOW"),
            directory.resolve("11112222-1m-low")
        );
    }

    #[test]
    fn duplicate_names_return_all_candidates_instead_of_picking_one() {
        let directory = ModelDirectory::new(
            &[builtin("11112222", "DeepSeek", "deepseek-chat")],
            &[plugin("33334444", "DeepSeek", "other-model")],
        )
        .unwrap();
        let Resolution::Ambiguous { candidates } = directory.resolve("DeepSeek") else {
            panic!("duplicate display names must be ambiguous")
        };
        let mut expected = vec!["11112222".to_string(), "33334444".to_string()];
        expected.sort();
        let mut got = candidates;
        got.sort();
        assert_eq!(got, expected);
        // 只有重名的那个名字歧义;上游模型名仍然唯一命中。
        assert_eq!(
            directory.resolve("other-model"),
            Resolution::Matched {
                id: "33334444".into(),
                parts: None
            }
        );
    }

    #[test]
    fn builtin_id_conflicts_are_rejected_with_both_sides_named() {
        let error = ModelDirectory::new(
            &[
                builtin("11112222", "Model A", "model-a"),
                builtin("11112222", "Model B", "model-b"),
            ],
            &[],
        )
        .unwrap_err();
        let message = error.to_string();
        assert!(message.contains("11112222"), "message: {message}");
        assert!(message.contains("Model A"), "message: {message}");
        assert!(message.contains("Model B"), "message: {message}");
    }

    #[test]
    fn cross_source_id_conflicts_are_rejected() {
        let error = ModelDirectory::new(
            &[builtin("11112222", "Builtin", "builtin-model")],
            &[plugin("11112222", "Plugin Model", "plugin-model")],
        )
        .unwrap_err();
        let message = error.to_string();
        assert!(message.contains("Builtin"), "message: {message}");
        assert!(message.contains("Plugin Model"), "message: {message}");
    }

    #[test]
    fn plugin_providers_cannot_share_an_identity() {
        let first = plugin("33334444", "First", "same-model");
        let mut second = first.clone();
        second.provider_id = "other-provider".into();
        assert!(ModelDirectory::new(&[], &[first, second]).is_err());
    }

    #[test]
    fn unknown_keys_are_not_found() {
        let directory = directory();
        assert_eq!(directory.resolve("unknown"), Resolution::NotFound);
        assert_eq!(directory.resolve(""), Resolution::NotFound);
        // 与任何 base 都不匹配的 slug。
        assert_eq!(directory.resolve("11112222-2m-low"), Resolution::NotFound);
    }

    #[test]
    fn entries_carry_source_and_plugin_reference() {
        let directory = directory();
        let builtin_entry = directory.get("11112222").unwrap();
        assert_eq!(builtin_entry.source, ModelSource::Builtin);
        assert!(builtin_entry.plugin.is_none());
        assert_eq!(builtin_entry.upstream_model_id, "deepseek-chat");
        let plugin_entry = directory.get("33334444").unwrap();
        assert_eq!(plugin_entry.source, ModelSource::Plugin);
        let plugin = plugin_entry.plugin.as_ref().unwrap();
        assert_eq!(plugin.provider_id, "codex");
        assert_eq!(plugin_entry.upstream_model_id, "org/gpt-5");
    }

    #[test]
    fn default_parts_fall_back_to_the_first_axis_option() {
        let directory = directory();
        let parts = directory.default_parts("11112222").unwrap();
        assert_eq!(parts.context, "200k");
        assert_eq!(parts.effort.as_deref(), Some("low"));
        assert!(directory.default_parts("missing").is_none());
    }
}
