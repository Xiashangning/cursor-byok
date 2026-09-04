//! Defines serializable plugin capability definitions and desktop descriptors.
use serde::{Deserialize, Serialize};

use super::state::{ResourceRecord, ResourceState, StoredModel};
use crate::store::PluginModelOverride;

/// 由 collect.ts 输出的能力摘要;不含任何可执行内容。
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PluginModuleDefinition {
    pub providers: Vec<ProviderDefinition>,
    #[serde(default)]
    pub resources: Vec<ResourceDefinition>,
}

/// 插件提供的显示文本:纯字符串或 locale → 文本映射;核心原样透传,由前端解析。
pub type LocalizedText = serde_json::Value;

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProviderDefinition {
    pub id: String,
    pub display_name: LocalizedText,
    #[serde(default)]
    pub description: LocalizedText,
    pub provider_type: String,
    #[serde(default)]
    pub resource_type: Option<String>,
    pub has_models: bool,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ResourceDefinition {
    #[serde(rename = "type")]
    pub resource_type: String,
    pub display_name: LocalizedText,
    #[serde(default)]
    pub add: Vec<AddMethodDefinition>,
    #[serde(default)]
    pub import: Option<ImportDefinition>,
    pub can_refresh: bool,
    pub can_remove: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AddMethodDefinition {
    #[serde(rename = "type")]
    pub method_type: String,
    pub id: String,
    pub display_name: LocalizedText,
    #[serde(default)]
    pub description: LocalizedText,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ImportDefinition {
    pub display_name: LocalizedText,
    #[serde(default)]
    pub description: LocalizedText,
    pub accept: Vec<String>,
    pub multiple: bool,
}

pub const OAUTH2_ADD_METHOD: &str = "oauth2.0";

/// 桌面端看到的插件全貌。
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginDescriptor {
    pub id: String,
    pub name: String,
    pub version: String,
    pub author: Option<String>,
    pub icon: String,
    pub providers: Vec<PluginProviderDescriptor>,
    pub resources: Vec<PluginResourceDescriptor>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginProviderDescriptor {
    pub id: String,
    pub plugin_id: String,
    pub display_name: LocalizedText,
    pub description: LocalizedText,
    pub provider_type: String,
    pub resource_type: Option<String>,
    pub has_models: bool,
    /// 已满足调用条件:模型目录非空,且需要资源时至少有一条资源。
    pub configured: bool,
    pub models: Vec<PluginModelDescriptor>,
}

/// 一个可直接被 Cursor 调用的插件模型。
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginModelDescriptor {
    /// 稳定模型 ID:`plugin:<plugin>/<provider>/<model>`。
    pub id: String,
    pub plugin_id: String,
    pub plugin_name: String,
    pub provider_id: String,
    pub model_id: String,
    pub display_name: String,
    pub description: Option<String>,
    pub icon: String,
    pub provider_type: String,
    pub max_output_tokens: Option<u64>,
    pub images: bool,
    /// 生效的 Effort 轴:宿主默认值,可被用户覆盖整体替换。
    pub effort_options: Vec<String>,
    /// 生效的 Context 档位轴:宿主默认值,可被用户覆盖整体替换。
    pub context_options: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginResourceDescriptor {
    #[serde(rename = "type")]
    pub resource_type: String,
    pub display_name: LocalizedText,
    pub add: Vec<AddMethodDefinition>,
    pub import: Option<ImportDefinition>,
    pub can_refresh: bool,
    pub can_remove: bool,
    pub resources: Vec<PluginResourceView>,
}

/// 单条资源的对外投影;凭证保留在核心存储,不进入该结构。
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginResourceView {
    pub id: String,
    pub state: ResourceState,
    pub display_name: String,
    pub description: LocalizedText,
    pub metrics: Vec<ResourceMetric>,
    pub created_at_ms: i64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ResourceMetric {
    pub id: String,
    pub label: LocalizedText,
    pub unit: String,
    pub value: f64,
    #[serde(default)]
    pub reset_at_ms: Option<i64>,
}

/// 插件对一条资源的展示投影(resource.present 的返回值)。
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ResourcePresentation {
    pub display_name: String,
    #[serde(default)]
    pub description: LocalizedText,
    #[serde(default)]
    pub metrics: Vec<ResourceMetric>,
}

impl PluginResourceView {
    pub fn from_record(record: &ResourceRecord, presentation: ResourcePresentation) -> Self {
        Self {
            id: record.id.clone(),
            state: record.state.clone(),
            display_name: presentation.display_name,
            description: presentation.description,
            metrics: presentation.metrics,
            created_at_ms: record.created_at_ms,
        }
    }
}

pub const ADAPTER_ID_PREFIX: &str = "plugin:";

pub fn model_id(plugin_id: &str, provider_id: &str, model_id: &str) -> String {
    format!("{ADAPTER_ID_PREFIX}{plugin_id}/{provider_id}/{model_id}")
}

/// 解析稳定模型 ID;上游模型段允许包含 `/`。
pub fn parse_model_id(value: &str) -> Option<(&str, &str, &str)> {
    let rest = value.strip_prefix(ADAPTER_ID_PREFIX)?;
    let (plugin_id, rest) = rest.split_once('/')?;
    let (provider_id, model_id) = rest.split_once('/')?;
    (!plugin_id.is_empty() && !provider_id.is_empty() && !model_id.is_empty()).then_some((
        plugin_id,
        provider_id,
        model_id,
    ))
}

/// 插件模型的默认 Effort 与 Context 档位轴;插件描述符不声明这两项,
/// 由宿主统一提供,用户覆盖可整体替换。
const DEFAULT_EFFORT_OPTIONS: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];
const DEFAULT_CONTEXT_OPTIONS: [&str; 5] = ["200k", "356k", "500k", "800k", "1m"];

impl PluginModelDescriptor {
    pub fn new(
        plugin_id: &str,
        plugin_name: &str,
        icon: &str,
        provider: &ProviderDefinition,
        model: &StoredModel,
    ) -> Self {
        Self {
            id: model_id(plugin_id, &provider.id, &model.id),
            plugin_id: plugin_id.to_owned(),
            plugin_name: plugin_name.to_owned(),
            provider_id: provider.id.clone(),
            model_id: model.id.clone(),
            display_name: model.display_name.clone(),
            description: model.description.clone(),
            icon: icon.to_owned(),
            provider_type: provider.provider_type.clone(),
            max_output_tokens: model.max_output_tokens,
            images: model.images,
            effort_options: DEFAULT_EFFORT_OPTIONS
                .iter()
                .map(|value| (*value).to_owned())
                .collect(),
            context_options: DEFAULT_CONTEXT_OPTIONS
                .iter()
                .map(|value| (*value).to_owned())
                .collect(),
        }
    }

    /// 把用户覆盖合并进描述符;None/空值表示该项保持默认。
    pub fn with_override(self, over: &PluginModelOverride) -> Self {
        let mut descriptor = self;
        if let Some(name) = over.display_name.as_deref().filter(|name| !name.is_empty()) {
            descriptor.display_name = name.to_owned();
        }
        if let Some(tooltip) = &over.tooltip {
            // tooltip 只作为 Cursor 的模型提示 markdown 消费,直接替换 description。
            descriptor.description = Some(tooltip.clone());
        }
        if let Some(options) = over
            .effort_options
            .as_deref()
            .filter(|options| !options.is_empty())
        {
            descriptor.effort_options = options.to_vec();
        }
        if let Some(options) = over
            .context_options
            .as_deref()
            .filter(|options| !options.is_empty())
        {
            descriptor.context_options = options.to_vec();
        }
        if over.max_output_tokens.is_some_and(|tokens| tokens > 0) {
            descriptor.max_output_tokens = over.max_output_tokens;
        }
        descriptor
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_stable_model_ids_with_slashes() {
        let id = model_id("dev.example", "codex", "org/gpt-5");
        assert_eq!(
            parse_model_id(&id),
            Some(("dev.example", "codex", "org/gpt-5"))
        );
        assert_eq!(parse_model_id("plugin:only/one"), None);
        assert_eq!(parse_model_id("model-hash"), None);
    }

    #[test]
    fn override_replaces_only_the_fields_it_provides() {
        let provider = ProviderDefinition {
            id: "codex".into(),
            display_name: serde_json::Value::Null,
            description: serde_json::Value::Null,
            provider_type: "openai".into(),
            resource_type: None,
            has_models: true,
        };
        let model = StoredModel {
            id: "gpt-5".into(),
            display_name: "GPT-5".into(),
            description: Some("plugin default".into()),
            max_output_tokens: None,
            images: false,
            private_data: serde_json::Value::Null,
        };
        let base = PluginModelDescriptor::new("dev.example", "Example", "", &provider, &model);

        let merged = base.clone().with_override(&PluginModelOverride {
            tooltip: Some("user tooltip".into()),
            ..PluginModelOverride::default()
        });
        assert_eq!(merged.display_name, "GPT-5");
        assert_eq!(merged.description.as_deref(), Some("user tooltip"));
        assert_eq!(merged.effort_options, base.effort_options);
        assert_eq!(merged.context_options, base.context_options);
        assert_eq!(merged.max_output_tokens, None);

        let merged = base.with_override(&PluginModelOverride {
            display_name: Some(String::new()),
            effort_options: Some(vec!["low".into()]),
            context_options: Some(vec!["1m".into()]),
            max_output_tokens: Some(65_536),
            ..PluginModelOverride::default()
        });
        // 空名称不生效(空白归一是写入时的职责),其余字段整体替换默认轴。
        assert_eq!(merged.display_name, "GPT-5");
        assert_eq!(merged.effort_options, vec!["low"]);
        assert_eq!(merged.context_options, vec!["1m"]);
        assert_eq!(merged.max_output_tokens, Some(65_536));
    }
}
