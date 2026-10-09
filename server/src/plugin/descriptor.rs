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
    pub has_notes: bool,
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
    #[serde(default)]
    pub actions: Vec<ResourceActionDefinition>,
    pub can_refresh: bool,
    pub can_remove: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ResourceActionDefinition {
    pub id: String,
    pub display_name: LocalizedText,
    #[serde(default)]
    pub description: LocalizedText,
    #[serde(default)]
    pub target: String,
    #[serde(default)]
    pub destructive: bool,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub callback: Option<OAuthCallbackDefinition>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct OAuthCallbackDefinition {
    #[serde(default)]
    pub port: Option<u16>,
    #[serde(default)]
    pub path: Option<String>,
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
pub const OAUTH2_AUTHORIZATION_CODE_ADD_METHOD: &str = "oauth2.authorization-code";

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
#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginModelDescriptor {
    /// 统一 8 位十六进制模型 ID。
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
    pub enabled: bool,
    /// 生效的 Effort 轴:宿主默认值,可被用户覆盖整体替换。
    pub effort_options: Vec<String>,
    /// 生效的 Context 档位轴:宿主默认值,可被用户覆盖整体替换。
    pub context_options: Vec<String>,
    /// 显式默认 Effort 档位(None 表示回退到 Effort 轴第一项)。
    pub default_effort: Option<String>,
    /// 显式默认 Context 档位(None 表示回退到 Context 轴第一项)。
    pub default_context: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginResourceDescriptor {
    #[serde(rename = "type")]
    pub resource_type: String,
    pub display_name: LocalizedText,
    pub add: Vec<AddMethodDefinition>,
    pub import: Option<ImportDefinition>,
    pub actions: Vec<ResourceActionDefinition>,
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
    pub tier: Option<String>,
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
    pub total: Option<f64>,
    #[serde(default)]
    pub reset_at_ms: Option<i64>,
}

/// 一条随额度 60s 刷新的模型备注(如限免、额度消耗倍率)。
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ModelNote {
    pub model_id: String,
    pub text: LocalizedText,
}

/// 插件对一条资源的展示投影(resource.present 的返回值)。
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ResourcePresentation {
    pub display_name: String,
    #[serde(default)]
    pub tier: Option<String>,
    #[serde(default)]
    pub metrics: Vec<ResourceMetric>,
}

/// 插件资源操作返回的安全详情;patch 只在核心内部应用,不会回传给桌面端。
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ResourceActionResult {
    pub title: LocalizedText,
    #[serde(default)]
    pub description: Option<LocalizedText>,
    #[serde(default)]
    pub cards: Vec<ResourceActionCard>,
    #[serde(default, skip_serializing)]
    pub patch: Option<super::state::ResourcePatch>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ResourceActionCard {
    pub id: String,
    pub title: LocalizedText,
    #[serde(default)]
    pub status: Option<LocalizedText>,
    #[serde(default)]
    pub granted_at_ms: Option<i64>,
    #[serde(default)]
    pub expires_at_ms: Option<i64>,
    #[serde(default)]
    pub fields: Vec<ResourceActionField>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ResourceActionField {
    pub id: String,
    pub label: LocalizedText,
    pub value: String,
}

/// 返回给桌面端的资源操作结果,明确排除插件私有 patch。
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ResourceActionResponse {
    pub title: LocalizedText,
    pub description: Option<LocalizedText>,
    pub cards: Vec<ResourceActionCard>,
}

impl From<ResourceActionResult> for ResourceActionResponse {
    fn from(result: ResourceActionResult) -> Self {
        Self {
            title: result.title,
            description: result.description,
            cards: result.cards,
        }
    }
}

impl PluginResourceView {
    pub fn from_record(record: &ResourceRecord, presentation: ResourcePresentation) -> Self {
        Self {
            id: record.id.clone(),
            state: record.state.clone(),
            display_name: presentation.display_name,
            tier: presentation.tier,
            metrics: presentation.metrics,
            created_at_ms: record.created_at_ms,
        }
    }
}

/// 插件模型的默认 Effort 与 Context 档位轴;插件描述符不声明这两项,
/// 由宿主统一提供,用户覆盖可整体替换。
use crate::model::{DEFAULT_CONTEXT_OPTIONS, DEFAULT_EFFORT_OPTIONS};

impl PluginModelDescriptor {
    pub fn new(
        plugin_id: &str,
        plugin_name: &str,
        icon: &str,
        provider: &ProviderDefinition,
        model: &StoredModel,
    ) -> Self {
        Self {
            id: crate::model::plugin_model_id(plugin_id, &model.id),
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
            enabled: true,
            effort_options: if model.effort_options.is_empty() {
                DEFAULT_EFFORT_OPTIONS
                    .iter()
                    .map(|value| (*value).to_owned())
                    .collect()
            } else {
                model.effort_options.clone()
            },
            context_options: if model.context_options.is_empty() {
                DEFAULT_CONTEXT_OPTIONS
                    .iter()
                    .map(|value| (*value).to_owned())
                    .collect()
            } else {
                model.context_options.clone()
            },
            default_effort: None,
            default_context: None,
        }
    }

    /// 目录同款变体轴:描述符的生效档位(已并入用户覆盖)。
    pub fn variant_axis(&self) -> crate::model::ModelVariantAxis {
        crate::model::ModelVariantAxis {
            context_options: self.context_options.clone(),
            effort_options: self.effort_options.clone(),
            default_context: self.default_context.clone(),
            default_effort: self.default_effort.clone(),
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
        if let Some(effort) = &over.default_effort {
            descriptor.default_effort = Some(effort.clone());
        }
        if let Some(context) = &over.default_context {
            descriptor.default_context = Some(context.clone());
        }
        descriptor
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descriptor_ids_use_the_unified_eight_character_identity() {
        let provider = ProviderDefinition {
            id: "codex".into(),
            display_name: serde_json::Value::Null,
            description: serde_json::Value::Null,
            provider_type: "openai".into(),
            resource_type: None,
            has_models: true,
            has_notes: false,
        };
        let model = StoredModel {
            id: "org/gpt-5".into(),
            display_name: "GPT-5".into(),
            description: None,
            max_output_tokens: None,
            images: false,
            effort_options: Vec::new(),
            context_options: Vec::new(),
            private_data: serde_json::Value::Null,
        };
        let descriptor =
            PluginModelDescriptor::new("dev.example", "Example", "", &provider, &model);
        assert_eq!(
            descriptor.id,
            crate::model::plugin_model_id("dev.example", "org/gpt-5")
        );
        assert_eq!(descriptor.model_id, "org/gpt-5");
        // ID 不再携带插件/供应商路由信息;执行信息保留在独立字段。
        assert_eq!(descriptor.provider_id, "codex");
        assert_eq!(descriptor.plugin_id, "dev.example");
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
            has_notes: false,
        };
        let model = StoredModel {
            id: "gpt-5".into(),
            display_name: "GPT-5".into(),
            description: Some("plugin default".into()),
            max_output_tokens: None,
            images: false,
            effort_options: Vec::new(),
            context_options: Vec::new(),
            private_data: serde_json::Value::Null,
        };
        let base = PluginModelDescriptor::new("dev.example", "Example", "", &provider, &model);
        assert_eq!(
            base.effort_options,
            DEFAULT_EFFORT_OPTIONS
                .iter()
                .map(|value| (*value).to_owned())
                .collect::<Vec<_>>()
        );

        let merged = base.clone().with_override(&PluginModelOverride {
            tooltip: Some("user tooltip".into()),
            ..PluginModelOverride::default()
        });
        assert_eq!(merged.display_name, "GPT-5");
        assert_eq!(merged.description.as_deref(), Some("user tooltip"));
        assert_eq!(merged.effort_options, base.effort_options);
        assert_eq!(merged.context_options, base.context_options);
        assert_eq!(merged.max_output_tokens, None);
        assert_eq!(merged.default_effort, None);
        assert_eq!(merged.default_context, None);

        let merged = base.with_override(&PluginModelOverride {
            display_name: Some(String::new()),
            effort_options: Some(vec!["low".into()]),
            context_options: Some(vec!["1m".into()]),
            max_output_tokens: Some(65_536),
            default_effort: Some("high".into()),
            default_context: Some("1m".into()),
            ..PluginModelOverride::default()
        });
        // 空名称不生效(空白归一是写入时的职责),其余字段整体替换默认轴。
        assert_eq!(merged.display_name, "GPT-5");
        assert_eq!(merged.effort_options, vec!["low"]);
        assert_eq!(merged.context_options, vec!["1m"]);
        assert_eq!(merged.max_output_tokens, Some(65_536));
        assert_eq!(merged.default_effort.as_deref(), Some("high"));
        assert_eq!(merged.default_context.as_deref(), Some("1m"));
    }

    #[test]
    fn plugin_options_replace_the_host_defaults() {
        let provider = ProviderDefinition {
            id: "qoder".into(),
            display_name: serde_json::Value::Null,
            description: serde_json::Value::Null,
            provider_type: "qoder".into(),
            resource_type: None,
            has_models: true,
            has_notes: false,
        };
        let model = StoredModel {
            id: "qmodel_38max".into(),
            display_name: "Qwen3.8-Max".into(),
            description: None,
            max_output_tokens: None,
            images: true,
            effort_options: vec!["low".into(), "medium".into(), "xhigh".into()],
            context_options: vec!["200k".into(), "400k".into(), "1m".into()],
            private_data: serde_json::Value::Null,
        };
        let base = PluginModelDescriptor::new("dev.example", "Example", "", &provider, &model);
        assert_eq!(base.effort_options, vec!["low", "medium", "xhigh"]);
        assert_eq!(base.context_options, vec!["200k", "400k", "1m"]);

        // 用户覆盖仍然优先于插件提供的档位。
        let merged = base.with_override(&PluginModelOverride {
            context_options: Some(vec!["1m".into()]),
            ..PluginModelOverride::default()
        });
        assert_eq!(merged.effort_options, vec!["low", "medium", "xhigh"]);
        assert_eq!(merged.context_options, vec!["1m"]);
    }
}
