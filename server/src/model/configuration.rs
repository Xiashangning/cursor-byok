//! Defines model and provider configuration.
use std::{fmt, str::FromStr};

use reqwest::Url;
use serde::{Deserialize, Serialize};
#[cfg(test)]
use sha2::{Digest, Sha256};

use crate::{Error, Result};

pub const OPENAI_RESPONSES_ENDPOINT: &str = "/v1/responses";
pub const OPENAI_CHAT_ENDPOINT: &str = "/v1/chat/completions";

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub enum ProviderType {
    #[serde(rename = "openai-chat")]
    OpenAiChat,
    #[serde(rename = "openai-responses")]
    OpenAiResponses,
    #[serde(rename = "anthropic")]
    Anthropic,
    /// 插件执行的调用;协议细节在插件内部,核心只按统一事件流记录。
    #[serde(rename = "plugin")]
    Plugin,
}

impl ProviderType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::OpenAiChat => "openai-chat",
            Self::OpenAiResponses => "openai-responses",
            Self::Anthropic => "anthropic",
            Self::Plugin => "plugin",
        }
    }
}

impl fmt::Display for ProviderType {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for ProviderType {
    type Err = Error;

    fn from_str(value: &str) -> Result<Self> {
        match value {
            "openai-chat" => Ok(Self::OpenAiChat),
            "openai-responses" => Ok(Self::OpenAiResponses),
            "anthropic" => Ok(Self::Anthropic),
            "plugin" => Ok(Self::Plugin),
            _ => Err(Error::Config(format!("unsupported provider type: {value}"))),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ModelType {
    #[default]
    OpenAi,
    Anthropic,
}

impl ModelType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::OpenAi => "openai",
            Self::Anthropic => "anthropic",
        }
    }
}

impl FromStr for ModelType {
    type Err = Error;

    fn from_str(value: &str) -> Result<Self> {
        match value {
            "openai" => Ok(Self::OpenAi),
            "anthropic" => Ok(Self::Anthropic),
            _ => Err(Error::Config(format!("unsupported model type: {value}"))),
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct ModelGroupEdit {
    /// 批量编辑目标的现有模型哈希;不存在时整批失败。
    pub model_hash: String,
    /// 缺省保持现值;空字符串清除分组名。
    pub group_name: Option<String>,
    /// 用户编辑后的请求地址;None 表示未编辑(保持现有值)。
    pub base_url: Option<String>,
    /// 用户编辑后的密钥;None 或脱敏占位符表示未修改,由服务端从现有配置回填。
    pub api_key: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ModelConfigInput {
    #[serde(default)]
    pub sort_order: i64,
    pub display_name: String,
    /// 供应商分组的自定义显示名;同一 base_url 主机下的模型共享。
    #[serde(default)]
    pub group_name: Option<String>,
    #[serde(rename = "type")]
    pub model_type: ModelType,
    pub base_url: String,
    #[serde(default)]
    pub use_full_url: bool,
    pub api_key: String,
    pub tooltip_data: String,
    pub model_id: String,
    #[serde(default)]
    pub default_context: Option<String>,
    #[serde(default)]
    pub default_effort: Option<String>,
    #[serde(default = "default_effort_options")]
    pub effort_options: Vec<String>,
    #[serde(default = "default_context_options")]
    pub context_options: Vec<String>,
    #[serde(default)]
    pub openai_endpoint: String,
    #[serde(default)]
    pub openai_extra_params_enabled: bool,
    #[serde(default = "empty_object")]
    pub openai_extra_params: serde_json::Value,
    #[serde(default)]
    pub custom_headers_enabled: bool,
    #[serde(default = "empty_object")]
    pub custom_headers: serde_json::Value,
    #[serde(default)]
    pub anthropic_extra_params_enabled: bool,
    #[serde(default = "empty_object")]
    pub anthropic_extra_params: serde_json::Value,
    pub context_window_tokens: Option<u64>,
    pub max_completion_tokens: Option<u64>,
    pub anthropic_max_tokens: Option<u64>,
    pub thinking_budget_tokens: Option<u64>,
}

#[derive(Clone, Debug, Serialize)]
pub struct ModelConfig {
    pub model_hash: String,
    pub sort_order: i64,
    pub display_name: String,
    pub group_name: Option<String>,
    #[serde(rename = "type")]
    pub model_type: ModelType,
    pub base_url: String,
    pub use_full_url: bool,
    pub api_key: String,
    pub tooltip_data: String,
    pub model_id: String,
    pub default_context: Option<String>,
    pub default_effort: Option<String>,
    pub effort_options: Vec<String>,
    pub context_options: Vec<String>,
    pub openai_endpoint: String,
    pub openai_extra_params_enabled: bool,
    pub openai_extra_params: serde_json::Value,
    pub custom_headers_enabled: bool,
    pub custom_headers: serde_json::Value,
    pub anthropic_extra_params_enabled: bool,
    pub anthropic_extra_params: serde_json::Value,
    pub context_window_tokens: Option<u64>,
    pub max_completion_tokens: Option<u64>,
    pub anthropic_max_tokens: Option<u64>,
    pub thinking_budget_tokens: Option<u64>,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

impl Default for ModelConfig {
    /// 测试夹具基线:JSON 字段必须是对象(Null 过不了 normalize 校验)。
    fn default() -> Self {
        Self {
            model_hash: String::new(),
            sort_order: 0,
            display_name: String::new(),
            group_name: None,
            model_type: ModelType::OpenAi,
            base_url: String::new(),
            use_full_url: false,
            api_key: String::new(),
            tooltip_data: String::new(),
            model_id: String::new(),
            default_context: None,
            default_effort: None,
            effort_options: Vec::new(),
            context_options: Vec::new(),
            openai_endpoint: String::new(),
            openai_extra_params_enabled: false,
            openai_extra_params: empty_object(),
            custom_headers_enabled: false,
            custom_headers: empty_object(),
            anthropic_extra_params_enabled: false,
            anthropic_extra_params: empty_object(),
            context_window_tokens: None,
            max_completion_tokens: None,
            anthropic_max_tokens: None,
            thinking_budget_tokens: None,
            created_at_ms: 0,
            updated_at_ms: 0,
        }
    }
}

impl ModelConfig {
    /// Control API responses expose configuration shape without returning credentials.
    /// 已保存的密钥与敏感头值替换为 REDACTED_SECRET 占位符(保留头名),非敏感头原样
    /// 返回以支持编辑器往返;更新时占位符视为「未修改」由服务端回填,空串视为「清除」。
    pub fn redact_secrets(mut self) -> Self {
        if !self.api_key.is_empty() {
            self.api_key = REDACTED_SECRET.into();
        }
        if let Some(headers) = self.custom_headers.as_object_mut() {
            for (name, value) in headers.iter_mut() {
                if is_sensitive_header(name)
                    && value.as_str().is_some_and(|value| !value.is_empty())
                {
                    *value = serde_json::Value::String(REDACTED_SECRET.into());
                }
            }
        }
        self
    }

    /// 还原为可再次提交的输入;哈希与时间戳不属于输入。
    pub fn into_input(self) -> ModelConfigInput {
        ModelConfigInput {
            sort_order: self.sort_order,
            display_name: self.display_name,
            group_name: self.group_name,
            model_type: self.model_type,
            base_url: self.base_url,
            use_full_url: self.use_full_url,
            api_key: self.api_key,
            tooltip_data: self.tooltip_data,
            model_id: self.model_id,
            default_context: self.default_context,
            default_effort: self.default_effort,
            effort_options: self.effort_options,
            context_options: self.context_options,
            openai_endpoint: self.openai_endpoint,
            openai_extra_params_enabled: self.openai_extra_params_enabled,
            openai_extra_params: self.openai_extra_params,
            custom_headers_enabled: self.custom_headers_enabled,
            custom_headers: self.custom_headers,
            anthropic_extra_params_enabled: self.anthropic_extra_params_enabled,
            anthropic_extra_params: self.anthropic_extra_params,
            context_window_tokens: self.context_window_tokens,
            max_completion_tokens: self.max_completion_tokens,
            anthropic_max_tokens: self.anthropic_max_tokens,
            thinking_budget_tokens: self.thinking_budget_tokens,
        }
    }

    pub fn provider_type(&self) -> ProviderType {
        match self.model_type {
            ModelType::Anthropic => ProviderType::Anthropic,
            ModelType::OpenAi if self.openai_endpoint == OPENAI_RESPONSES_ENDPOINT => {
                ProviderType::OpenAiResponses
            }
            ModelType::OpenAi => ProviderType::OpenAiChat,
        }
    }

    pub fn request_url(&self) -> Result<String> {
        resolve_request_url(
            self.model_type,
            &self.base_url,
            &self.openai_endpoint,
            self.use_full_url,
        )
    }

    pub fn max_output_tokens(&self) -> Option<u64> {
        match self.model_type {
            ModelType::OpenAi => self.max_completion_tokens,
            ModelType::Anthropic => self.anthropic_max_tokens.or(self.max_completion_tokens),
        }
    }

    /// 缺上游模型名的配置是可编辑草稿;执行与目录发布都拒绝草稿。
    pub fn is_draft(&self) -> bool {
        self.model_id.trim().is_empty()
    }

    pub fn extra_params(&self) -> &serde_json::Value {
        match self.model_type {
            ModelType::OpenAi if self.openai_extra_params_enabled => &self.openai_extra_params,
            ModelType::Anthropic if self.anthropic_extra_params_enabled => {
                &self.anthropic_extra_params
            }
            _ => empty_object_ref(),
        }
    }

    pub fn configure(&self, model: &mut super::ModelSpec) {
        model.display_name = Some(self.display_name.clone());
        model.thinking_budget_tokens = self.thinking_budget_tokens;
        // A request-selected context is authoritative.  Use the saved model
        // value only when Cursor did not send a context parameter.
        if model.context_window_tokens.is_none() {
            model.context_window_tokens = self.context_window_tokens.or_else(|| {
                self.default_context
                    .as_deref()
                    .and_then(super::parse_token_count)
            });
        }
        if model.reasoning.explicitly_disabled {
            model.reasoning.enabled = false;
            model.reasoning.effort = None;
            return;
        }
        if model.reasoning.effort.is_none() {
            model.reasoning.effort = self.default_effort.clone();
        }
        model.reasoning.enabled |= model.reasoning.effort.is_some()
            || (self.model_type == ModelType::Anthropic && self.thinking_budget_tokens.is_some());
    }

    /// 目录同款变体轴:配置窗口对应的命名项(或裸 token 数)在 context 轴最前,
    /// 其余保持配置顺序;effort 轴原样采用配置(可以为空,表示无推理轴)。
    /// 显式默认档位由 default_parts 原样优先采用(不做轴成员校验)。
    pub fn variant_axis(&self) -> ModelVariantAxis {
        let mut context_options = Vec::with_capacity(
            self.context_options.len() + usize::from(self.context_window_tokens.is_some()),
        );
        if let Some(tokens) = self.context_window_tokens {
            match self
                .context_options
                .iter()
                .find(|value| super::parse_token_count(value) == Some(tokens))
            {
                Some(value) => context_options.push(value.clone()),
                None => context_options.push(tokens.to_string()),
            }
        }
        for value in &self.context_options {
            if self
                .context_window_tokens
                .is_some_and(|tokens| super::parse_token_count(value) == Some(tokens))
            {
                continue;
            }
            context_options.push(value.clone());
        }
        ModelVariantAxis {
            context_options,
            effort_options: self.effort_options.clone(),
            default_context: self.default_context.clone(),
            default_effort: self.default_effort.clone(),
        }
    }
}

/// 变体 slug 的分量:{hash}-{context}[-{effort}][-fast];
/// 无推理轴的模型 slug 不含 effort 段。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ModelVariantParts {
    pub context: String,
    pub effort: Option<String>,
    pub fast: bool,
}

/// 一个模型的变体轴。目录发布、slug 解析与 Task 工具默认值共用同一口径。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ModelVariantAxis {
    pub context_options: Vec<String>,
    pub effort_options: Vec<String>,
    /// 显式默认 Context 档位;default_parts 优先原样采用。
    pub default_context: Option<String>,
    /// 显式默认 Effort 档位;default_parts 优先原样采用。
    pub default_effort: Option<String>,
}

impl ModelVariantAxis {
    /// 默认变体口径:context 取显式 default_context,否则取轴第一项;effort 轴非空时
    /// 取显式 default_effort,否则取轴第一项,轴空则为 None。显式默认不做轴成员校验,
    /// 原样采用。无 context 轴且无显式默认时整体为 None(无法烘焙 slug)。
    pub fn default_parts(&self) -> Option<ModelVariantParts> {
        let context = self
            .default_context
            .clone()
            .or_else(|| self.context_options.first().cloned())?;
        let effort = if self.effort_options.is_empty() {
            None
        } else {
            Some(
                self.default_effort
                    .clone()
                    .unwrap_or_else(|| self.effort_options[0].clone()),
            )
        };
        Some(ModelVariantParts {
            context,
            effort,
            fast: false,
        })
    }

    /// 解析变体表示:连字符 slug {hash}-{context}[-{effort}][-fast] 与目录发布的
    /// 括号表示 {hash}[context=..,reasoning=..,fast=..] 都接受,校验口径一致。
    pub fn parse_slug(&self, hash: &str, key: &str) -> Option<ModelVariantParts> {
        if let Some(body) = key
            .strip_prefix(hash)
            .and_then(|rest| rest.strip_prefix('['))
            .and_then(|rest| rest.strip_suffix(']'))
        {
            return self.parse_variant_body(body);
        }
        let suffix = key.strip_prefix(hash)?.strip_prefix('-')?;
        let (suffix, fast) = match suffix.strip_suffix("-fast") {
            Some(suffix) => (suffix, true),
            None => (suffix, false),
        };
        let (context, effort) = if self.effort_options.is_empty() {
            (suffix, None)
        } else {
            let (context, effort) = suffix.rsplit_once('-')?;
            (context, Some(effort))
        };
        if !self.accepts_context(context) {
            return None;
        }
        if let Some(effort) = effort {
            if !self.accepts_effort(effort) {
                return None;
            }
        }
        Some(ModelVariantParts {
            context: context.into(),
            effort: effort.map(str::to_string),
            fast,
        })
    }

    /// 括号体内是逗号分隔的 id=value 对;context 必须存在,未知 id 忽略。
    fn parse_variant_body(&self, body: &str) -> Option<ModelVariantParts> {
        let mut context = None;
        let mut effort = None;
        let mut fast = false;
        for pair in body.split(',') {
            let (id, value) = pair.split_once('=')?;
            match id {
                "context" => context = Some(value),
                "reasoning" | "effort" => effort = Some(value),
                "fast" => fast = value == "true",
                _ => {}
            }
        }
        let context = context?;
        if !self.accepts_context(context) {
            return None;
        }
        if let Some(effort) = effort {
            if !self.accepts_effort(effort) {
                return None;
            }
        }
        Some(ModelVariantParts {
            context: context.into(),
            effort: effort.map(str::to_string),
            fast,
        })
    }

    fn accepts_context(&self, context: &str) -> bool {
        self.context_options.iter().any(|value| value == context)
            || (context.bytes().all(|byte| byte.is_ascii_digit())
                && context.parse::<u64>().is_ok_and(|tokens| tokens > 0))
    }

    fn accepts_effort(&self, effort: &str) -> bool {
        !self.effort_options.is_empty()
            && (self.effort_options.iter().any(|value| value == effort)
                || matches!(effort, "none" | "off"))
    }

    /// 烘焙变体 slug;无推理轴时不含 effort 段。
    pub fn bake_slug(&self, hash: &str, parts: &ModelVariantParts) -> String {
        let mut slug = format!("{hash}-{}", parts.context);
        if !self.effort_options.is_empty() {
            if let Some(effort) = &parts.effort {
                slug = format!("{slug}-{effort}");
            }
        }
        if parts.fast {
            slug = format!("{slug}-fast");
        }
        slug
    }

    /// 窗口 token 数对应的 context 选项;无匹配命名项时回退裸 token 数。
    pub fn context_option_for_tokens(&self, tokens: u64) -> Option<String> {
        Some(
            self.context_options
                .iter()
                .find(|value| super::parse_token_count(value) == Some(tokens))
                .cloned()
                .unwrap_or_else(|| tokens.to_string()),
        )
    }

    /// Task 工具 context 参数校验:归一小写后必须落在轴上(允许等值 token 数写法)。
    pub fn validate_context(&self, value: &str) -> Result<String> {
        let value = value.trim().to_ascii_lowercase();
        if self.context_options.is_empty() {
            return Err(Error::Protocol(
                "Task model parameter context is not supported by this model".into(),
            ));
        }
        self.context_options
            .iter()
            .find(|option| option.as_str() == value)
            .or_else(|| {
                let tokens = super::parse_token_count(&value)?;
                self.context_options
                    .iter()
                    .find(|option| super::parse_token_count(option) == Some(tokens))
            })
            .cloned()
            .ok_or_else(|| {
                Error::Protocol(format!(
                    "Task model parameter context must be one of: {}",
                    self.context_options.join(", ")
                ))
            })
    }

    /// Task 工具 reasoning 参数校验:归一小写后必须落在轴上。
    pub fn validate_effort(&self, value: &str) -> Result<String> {
        let value = value.trim().to_ascii_lowercase();
        if self.effort_options.is_empty() {
            return Err(Error::Protocol(
                "Task model parameter reasoning is not supported by this model".into(),
            ));
        }
        if self.effort_options.iter().any(|option| option == &value) {
            Ok(value)
        } else {
            Err(Error::Protocol(format!(
                "Task model parameter reasoning must be one of: {}",
                self.effort_options.join(", ")
            )))
        }
    }
}

pub fn normalize_model_input(input: &ModelConfigInput) -> Result<ModelConfigInput> {
    let display_name = required(&input.display_name, "model display name")?;
    let group_name = input
        .group_name
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(String::from);
    let base_url = normalize_request_url(&input.base_url)?;
    let api_key = required(&input.api_key, "model API key")?;
    let tooltip_data = required(&input.tooltip_data, "model tooltip")?;
    // Empty upstream IDs are editable drafts; execution rejects them.
    let model_id = input.model_id.trim().to_owned();
    // Effort 轴与默认档位只在保存入口归一(与插件用户覆盖同规则):白名单
    // 过滤、去重保序,空轴/全无效回退默认五档;默认档位不在最终轴上时取
    // 轴首项而非报错。读取与运行期目录轴不清洗,旧库非法配置下次保存时自愈。
    let (effort_options, default_effort) =
        normalize_effort_axis(&input.effort_options, input.default_effort.as_deref());
    let context_options = input.context_options.clone();
    // context 轴首项是配置窗口的轴表示(见 variant_axis):窗口存在时默认档位
    // 必须与之一致,否则默认变体会把配置窗口顶掉;无窗口时退到选项首项。
    let default_context = input
        .default_context
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| value.to_ascii_lowercase())
        .or_else(|| {
            input.context_window_tokens.map(|tokens| {
                context_options
                    .iter()
                    .find(|value| super::parse_token_count(value) == Some(tokens))
                    .cloned()
                    .unwrap_or_else(|| tokens.to_string())
            })
        })
        .or_else(|| context_options.first().cloned())
        .or_else(|| Some("200k".into()));
    let openai_endpoint = match input.model_type {
        ModelType::OpenAi => normalize_openai_endpoint(&input.openai_endpoint)?,
        ModelType::Anthropic => String::new(),
    };
    validate_object(&input.openai_extra_params, "OpenAI extra params")?;
    validate_object(&input.anthropic_extra_params, "Anthropic extra params")?;
    validate_headers(&input.custom_headers)?;

    let normalized = ModelConfigInput {
        sort_order: input.sort_order.max(0),
        display_name,
        group_name,
        model_type: input.model_type,
        base_url,
        use_full_url: input.use_full_url,
        api_key,
        tooltip_data,
        model_id,
        default_context,
        default_effort: Some(default_effort),
        effort_options,
        context_options,
        openai_endpoint,
        openai_extra_params_enabled: input.model_type == ModelType::OpenAi
            && input.openai_extra_params_enabled,
        openai_extra_params: if input.model_type == ModelType::OpenAi {
            input.openai_extra_params.clone()
        } else {
            empty_object()
        },
        custom_headers_enabled: input.custom_headers_enabled,
        custom_headers: input.custom_headers.clone(),
        anthropic_extra_params_enabled: input.model_type == ModelType::Anthropic
            && input.anthropic_extra_params_enabled,
        anthropic_extra_params: if input.model_type == ModelType::Anthropic {
            input.anthropic_extra_params.clone()
        } else {
            empty_object()
        },
        context_window_tokens: positive(input.context_window_tokens, "context window")?,
        max_completion_tokens: positive(input.max_completion_tokens, "max completion tokens")?,
        anthropic_max_tokens: positive(input.anthropic_max_tokens, "Anthropic max tokens")?,
        thinking_budget_tokens: positive(input.thinking_budget_tokens, "thinking budget")?,
    };
    resolve_request_url(
        normalized.model_type,
        &normalized.base_url,
        &normalized.openai_endpoint,
        normalized.use_full_url,
    )?;
    Ok(normalized)
}

pub fn model_hash(input: &ModelConfigInput) -> Result<String> {
    let normalized = normalize_model_input(input)?;
    super::identity::builtin_model_id(&normalized)
}

pub fn normalize_request_url(value: &str) -> Result<String> {
    let value = value.trim();
    let url = Url::parse(value)
        .map_err(|error| Error::Config(format!("invalid model request URL: {error}")))?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Err(Error::Config(
            "model request URL must be an HTTP(S) URL with a host".into(),
        ));
    }
    if url.fragment().is_some() {
        return Err(Error::Config(
            "model request URL cannot contain a fragment".into(),
        ));
    }
    Ok(value.into())
}

pub fn resolve_request_url(
    model_type: ModelType,
    base_url: &str,
    openai_endpoint: &str,
    use_full_url: bool,
) -> Result<String> {
    let base_url = normalize_request_url(base_url)?;
    let endpoint = match model_type {
        ModelType::OpenAi => normalize_openai_endpoint(openai_endpoint)?,
        ModelType::Anthropic => "/v1/messages".into(),
    };
    if use_full_url {
        return Ok(base_url);
    }
    append_standard_endpoint(&base_url, &endpoint)
}

fn append_standard_endpoint(base_url: &str, endpoint: &str) -> Result<String> {
    let mut url = Url::parse(base_url)
        .map_err(|error| Error::Config(format!("invalid model server URL: {error}")))?;
    let base_path = url.path().trim_end_matches('/').to_string();
    let endpoint = if has_trailing_version(&base_path) {
        endpoint.strip_prefix("/v1").unwrap_or(endpoint)
    } else {
        endpoint
    };
    url.set_path(&format!("{base_path}{endpoint}"));
    normalize_request_url(url.as_str())
}

pub(crate) fn has_trailing_version(path: &str) -> bool {
    let Some(segment) = path.rsplit('/').next() else {
        return false;
    };
    segment.strip_prefix('v').is_some_and(|digits| {
        !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit())
    })
}

pub fn is_sensitive_header(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "authorization" | "proxy-authorization" | "x-api-key" | "api-key" | "cookie" | "set-cookie"
    )
}

/// 脱敏占位符:控制 API 用它替换已保存的密钥与敏感头值。编辑器原样往返
/// 即「未修改」(服务端回填),传回空串即「清除」;它不代表任何真实密钥内容。
pub const REDACTED_SECRET: &str = "••••••••";

pub(super) fn normalize_openai_endpoint(value: &str) -> Result<String> {
    match value.trim() {
        "" | OPENAI_RESPONSES_ENDPOINT => Ok(OPENAI_RESPONSES_ENDPOINT.into()),
        OPENAI_CHAT_ENDPOINT => Ok(OPENAI_CHAT_ENDPOINT.into()),
        value => Err(Error::Config(format!(
            "unsupported OpenAI endpoint: {value}"
        ))),
    }
}

/// Effort 取值的全局白名单;标准拼写 `xhigh`。每个模型各自保存其中的一
/// 个子集,运行期按轴成员口径消费;这里只做保存入口的取值域过滤。
pub(crate) const EFFORT_WHITELIST: &[&str] =
    &["none", "minimal", "low", "medium", "high", "xhigh", "max"];

/// 保存入口的 Effort 轴归一,内置模型与插件用户覆盖共用同一规则:
/// 去空白、转小写、过滤白名单外的取值、去重保序;过滤后为空时回退
/// `DEFAULT_EFFORT_OPTIONS`。默认档位在最终轴上则保留,否则取轴首项。
/// 只产生有效配置,永不因取值报错;读取与运行期目录轴不清洗,旧库的
/// 非法配置在下次保存时自愈。
pub(crate) fn normalize_effort_axis(
    effort_options: &[String],
    default_effort: Option<&str>,
) -> (Vec<String>, String) {
    let mut options = Vec::with_capacity(effort_options.len());
    for value in effort_options {
        let value = value.trim().to_ascii_lowercase();
        if EFFORT_WHITELIST.contains(&value.as_str()) && !options.contains(&value) {
            options.push(value);
        }
    }
    if options.is_empty() {
        options = DEFAULT_EFFORT_OPTIONS
            .iter()
            .map(|value| (*value).into())
            .collect();
    }
    let default = default_effort
        .map(|value| value.trim().to_ascii_lowercase())
        .filter(|value| options.contains(value))
        .unwrap_or_else(|| options[0].clone());
    (options, default)
}

/// 无 Effort 覆盖保持 None;一旦指定轴或默认值,就保存完整的归一轴。
/// 仅覆盖默认值时,控制层先补齐模型当前轴,避免脱离其有效子集。
pub(crate) fn normalize_plugin_effort_override(
    effort_options: Option<Vec<String>>,
    default_effort: Option<String>,
) -> (Option<Vec<String>>, Option<String>) {
    if effort_options.is_none() && default_effort.is_none() {
        return (None, None);
    }
    let (options, default) = normalize_effort_axis(
        &effort_options.unwrap_or_default(),
        default_effort.as_deref(),
    );
    (Some(options), Some(default))
}

fn positive(value: Option<u64>, label: &str) -> Result<Option<u64>> {
    match value {
        Some(0) => Err(Error::Config(format!("{label} must be greater than zero"))),
        value => Ok(value),
    }
}

fn required(value: &str, label: &str) -> Result<String> {
    let value = value.trim();
    if value.is_empty() {
        Err(Error::Config(format!("{label} cannot be empty")))
    } else {
        Ok(value.into())
    }
}

fn validate_object(value: &serde_json::Value, label: &str) -> Result<()> {
    if value.is_object() {
        Ok(())
    } else {
        Err(Error::Config(format!("{label} must be a JSON object")))
    }
}

fn validate_headers(value: &serde_json::Value) -> Result<()> {
    validate_object(value, "custom headers")?;
    for (name, value) in value.as_object().expect("validated object") {
        if name.trim().is_empty() || !value.is_string() {
            return Err(Error::Config(
                "custom headers must have non-empty names and string values".into(),
            ));
        }
    }
    Ok(())
}

fn empty_object() -> serde_json::Value {
    serde_json::json!({})
}

pub const DEFAULT_EFFORT_OPTIONS: &[&str] = &["low", "medium", "high", "xhigh", "max"];
pub const DEFAULT_CONTEXT_OPTIONS: &[&str] = &["200k", "356k", "800k", "1m"];

fn default_effort_options() -> Vec<String> {
    DEFAULT_EFFORT_OPTIONS
        .iter()
        .map(|value| (*value).into())
        .collect()
}

fn default_context_options() -> Vec<String> {
    DEFAULT_CONTEXT_OPTIONS
        .iter()
        .map(|value| (*value).into())
        .collect()
}

fn empty_object_ref() -> &'static serde_json::Value {
    static EMPTY: std::sync::OnceLock<serde_json::Value> = std::sync::OnceLock::new();
    EMPTY.get_or_init(empty_object)
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReasoningSpec {
    pub enabled: bool,
    pub effort: Option<String>,
    #[serde(default)]
    pub explicitly_disabled: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ModelLatency {
    #[default]
    Standard,
    Fast,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ModelSpec {
    pub model_id: String,
    pub display_name: Option<String>,
    pub reasoning: ReasoningSpec,
    pub latency: ModelLatency,
    pub max_output_tokens: Option<u64>,
    pub context_window_tokens: Option<u64>,
    #[serde(default)]
    pub thinking_budget_tokens: Option<u64>,
    #[serde(default)]
    pub supports_image_generation: bool,
    #[serde(default)]
    pub extra_params: serde_json::Value,
}

impl ModelSpec {
    pub fn new(model_id: impl Into<String>) -> Self {
        Self {
            model_id: model_id.into(),
            display_name: None,
            reasoning: ReasoningSpec::default(),
            latency: ModelLatency::Standard,
            max_output_tokens: None,
            context_window_tokens: None,
            thinking_budget_tokens: None,
            supports_image_generation: false,
            extra_params: serde_json::json!({}),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input() -> ModelConfigInput {
        ModelConfigInput {
            sort_order: 1,
            display_name: "Model A".into(),
            group_name: None,
            model_type: ModelType::OpenAi,
            base_url: "https://example.com/custom/generate".into(),
            use_full_url: true,
            api_key: "secret".into(),
            tooltip_data: "Model A".into(),
            model_id: "model-a".into(),
            default_context: None,
            default_effort: Some("high".into()),
            effort_options: default_effort_options(),
            context_options: default_context_options(),
            openai_endpoint: OPENAI_RESPONSES_ENDPOINT.into(),
            openai_extra_params_enabled: false,
            openai_extra_params: empty_object(),
            custom_headers_enabled: false,
            custom_headers: empty_object(),
            anthropic_extra_params_enabled: false,
            anthropic_extra_params: empty_object(),
            context_window_tokens: Some(200_000),
            max_completion_tokens: None,
            anthropic_max_tokens: None,
            thinking_budget_tokens: None,
        }
    }

    fn config(input: &ModelConfigInput) -> ModelConfig {
        ModelConfig {
            model_hash: "hash".into(),
            sort_order: input.sort_order,
            display_name: input.display_name.clone(),
            group_name: None,
            model_type: input.model_type,
            base_url: input.base_url.clone(),
            use_full_url: input.use_full_url,
            api_key: input.api_key.clone(),
            tooltip_data: input.tooltip_data.clone(),
            model_id: input.model_id.clone(),
            default_context: input.default_context.clone(),
            default_effort: input.default_effort.clone(),
            effort_options: input.effort_options.clone(),
            context_options: input.context_options.clone(),
            openai_endpoint: input.openai_endpoint.clone(),
            openai_extra_params_enabled: input.openai_extra_params_enabled,
            openai_extra_params: input.openai_extra_params.clone(),
            custom_headers_enabled: input.custom_headers_enabled,
            custom_headers: input.custom_headers.clone(),
            anthropic_extra_params_enabled: input.anthropic_extra_params_enabled,
            anthropic_extra_params: input.anthropic_extra_params.clone(),
            context_window_tokens: input.context_window_tokens,
            max_completion_tokens: input.max_completion_tokens,
            anthropic_max_tokens: input.anthropic_max_tokens,
            thinking_budget_tokens: input.thinking_budget_tokens,
            created_at_ms: 0,
            updated_at_ms: 0,
        }
    }

    #[test]
    fn a_thinking_budget_flows_to_the_spec_and_enables_reasoning_for_anthropic_only() {
        let mut anthropic = input();
        anthropic.model_type = ModelType::Anthropic;
        anthropic.default_effort = None;
        anthropic.thinking_budget_tokens = Some(8_000);
        let mut spec = super::super::ModelSpec::new("model-a");

        config(&anthropic).configure(&mut spec);

        assert_eq!(spec.thinking_budget_tokens, Some(8_000));
        assert!(spec.reasoning.enabled);
        assert_eq!(spec.reasoning.effort, None);

        // OpenAI 模型不消费该字段,也不应因此开启 reasoning。
        let mut openai = input();
        openai.default_effort = None;
        openai.thinking_budget_tokens = Some(8_000);
        let mut spec = super::super::ModelSpec::new("model-a");

        config(&openai).configure(&mut spec);

        assert!(!spec.reasoning.enabled);

        // 显式关闭 reasoning 优先于预算。
        let mut spec = super::super::ModelSpec::new("model-a");
        spec.reasoning.explicitly_disabled = true;

        config(&anthropic).configure(&mut spec);

        assert!(!spec.reasoning.enabled);
    }

    #[test]
    fn group_patch_distinguishes_omitted_and_cleared_names() {
        let omitted: ModelGroupEdit =
            serde_json::from_value(serde_json::json!({"model_hash":"id"})).unwrap();
        let cleared: ModelGroupEdit =
            serde_json::from_value(serde_json::json!({"model_hash":"id", "group_name":""}))
                .unwrap();
        assert_eq!(omitted.group_name, None);
        assert_eq!(cleared.group_name, Some(String::new()));
    }

    #[test]
    fn normalize_model_input_lowercases_effort_options() {
        let mut input = input();
        input.effort_options = vec!["LOW".into(), "High".into()];

        let normalized = normalize_model_input(&input).unwrap();

        assert_eq!(normalized.effort_options, vec!["low", "high"]);
    }

    /// 保存入口的 Effort 归一(M-1):白名单过滤、去重保序、空轴回填默认
    /// 五档;默认档位无效时取最终轴首项,不再按旧白名单报 400。
    #[test]
    fn effort_axis_normalizes_custom_options_and_anchors_the_default() {
        // 自定义轴首项(旧锁死复现):保存成功,无效项被过滤,默认取首个有效项。
        let mut input = input();
        input.effort_options = vec!["turbo".into(), "low".into(), "HIGH ".into()];
        input.default_effort = Some("turbo".into());
        let normalized = normalize_model_input(&input).unwrap();
        assert_eq!(normalized.effort_options, vec!["low", "high"]);
        assert_eq!(normalized.default_effort, Some("low".into()));

        // null 默认 + 自定义轴首:回退首个有效项(隐式落库锁死的另一形态)。
        input.default_effort = None;
        let normalized = normalize_model_input(&input).unwrap();
        assert_eq!(normalized.default_effort, Some("low".into()));

        // 全无效轴:回填默认五档,默认取首项 low。
        input.effort_options = vec!["turbo".into(), "  ".into()];
        let normalized = normalize_model_input(&input).unwrap();
        assert_eq!(normalized.effort_options, default_effort_options());
        assert_eq!(normalized.default_effort, Some("low".into()));

        // 空轴同样回填五档。
        input.effort_options = Vec::new();
        let normalized = normalize_model_input(&input).unwrap();
        assert_eq!(normalized.effort_options, default_effort_options());

        // 有效默认保留;大小写/空白归一;重复项保序去重。
        input.effort_options = vec![
            " high ".into(),
            "High".into(),
            "XHIGH".into(),
            "none".into(),
            "minimal".into(),
        ];
        input.default_effort = Some(" HIGH ".into());
        let normalized = normalize_model_input(&input).unwrap();
        assert_eq!(
            normalized.effort_options,
            vec!["high", "xhigh", "none", "minimal"]
        );
        assert_eq!(normalized.default_effort, Some("high".into()));

        // 默认不在最终轴上(虽在白名单内):取轴首项而非报错。
        input.default_effort = Some("low".into());
        let normalized = normalize_model_input(&input).unwrap();
        assert_eq!(normalized.default_effort, Some("high".into()));
    }

    /// 插件用户覆盖与内置模型同规则(接入点在 settings 的 normalized())。
    #[test]
    fn plugin_effort_override_normalizes_like_builtin_saves() {
        let axis = |values: &[&str]| {
            values
                .iter()
                .map(|value| (*value).into())
                .collect::<Vec<String>>()
        };

        // 提供轴:过滤、去重、回填、默认锚定与内置一致。
        let (options, default) = normalize_plugin_effort_override(
            Some(axis(&["turbo", "low", "LOW"])),
            Some("turbo".into()),
        );
        assert_eq!(options, Some(axis(&["low"])));
        assert_eq!(default, Some("low".into()));

        // 显式空轴与全无效轴均回填五档。
        let (options, default) =
            normalize_plugin_effort_override(Some(Vec::new()), Some("turbo".into()));
        assert_eq!(options, Some(default_effort_options()));
        assert_eq!(default, Some("low".into()));
        let (options, default) = normalize_plugin_effort_override(None, Some(" HIGH ".into()));
        assert_eq!(options, Some(default_effort_options()));
        assert_eq!(default, Some("high".into()));
        assert_eq!(normalize_plugin_effort_override(None, None), (None, None));

        // 全无效轴回填默认五档(与内置一致)。
        let (options, default) = normalize_plugin_effort_override(Some(axis(&["turbo"])), None);
        assert_eq!(options, Some(default_effort_options()));
        assert_eq!(default, Some("low".into()));

        assert_eq!(
            EFFORT_WHITELIST,
            ["none", "minimal", "low", "medium", "high", "xhigh", "max"]
        );
    }

    #[test]
    fn normalize_default_context_trims_lowercases_and_falls_back_to_the_first_axis_option() {
        let mut input = input();
        input.default_context = Some("  1M ".into());
        assert_eq!(
            normalize_model_input(&input).unwrap().default_context,
            Some("1m".into())
        );
        input.default_context = Some("   ".into());
        assert_eq!(
            normalize_model_input(&input).unwrap().default_context,
            Some("200k".into())
        );
        // 不做轴成员校验。
        input.default_context = Some("2m".into());
        assert_eq!(
            normalize_model_input(&input).unwrap().default_context,
            Some("2m".into())
        );
        // 窗口存在时默认档位是窗口的轴表示,与 variant_axis 首项一致。
        input.default_context = None;
        input.context_window_tokens = Some(100_000);
        assert_eq!(
            normalize_model_input(&input).unwrap().default_context,
            Some("100000".into())
        );
        // 窗口命中命名项时用命名项;无窗口且轴为空的极端情况回退 '200k'。
        input.context_window_tokens = Some(200_000);
        assert_eq!(
            normalize_model_input(&input).unwrap().default_context,
            Some("200k".into())
        );
        input.context_window_tokens = None;
        input.context_options = Vec::new();
        assert_eq!(
            normalize_model_input(&input).unwrap().default_context,
            Some("200k".into())
        );
    }

    #[test]
    fn hash_covers_url_model_and_key() {
        let original = input();
        let expected = Sha256::digest(
            "openai-responses\nhttps://example.com/custom/generate\nmodel-a\nsecret".as_bytes(),
        );
        assert_eq!(model_hash(&original).unwrap(), hex::encode(&expected[..4]));
    }

    #[test]
    fn request_url_is_exact_and_protocol_does_not_depend_on_its_path() {
        assert_eq!(
            resolve_request_url(
                ModelType::OpenAi,
                "https://example.com/custom/generate?api-version=2026-01-01",
                OPENAI_RESPONSES_ENDPOINT,
                true,
            )
            .unwrap(),
            "https://example.com/custom/generate?api-version=2026-01-01"
        );
        assert_eq!(
            resolve_request_url(
                ModelType::OpenAi,
                "https://example.com/another/arbitrary/path",
                OPENAI_CHAT_ENDPOINT,
                true,
            )
            .unwrap(),
            "https://example.com/another/arbitrary/path"
        );
        assert_eq!(
            resolve_request_url(ModelType::Anthropic, "https://example.com/claude", "", true)
                .unwrap(),
            "https://example.com/claude"
        );
        assert_eq!(
            resolve_request_url(
                ModelType::Anthropic,
                "https://example.com/claude/",
                "",
                true
            )
            .unwrap(),
            "https://example.com/claude/"
        );
        assert_eq!(
            resolve_request_url(
                ModelType::OpenAi,
                "https://example.com/v1",
                OPENAI_RESPONSES_ENDPOINT,
                false,
            )
            .unwrap(),
            "https://example.com/v1/responses"
        );
        assert_eq!(
            resolve_request_url(ModelType::Anthropic, "https://example.com/v1", "", false).unwrap(),
            "https://example.com/v1/messages"
        );
    }

    #[test]
    fn configured_context_window_does_not_override_the_client_request() {
        let input = input();
        let mut config = config(&input);
        config.context_window_tokens = Some(350_000);
        let mut requested = super::super::ModelSpec::new("model-a");
        requested.context_window_tokens = Some(200_000);

        config.configure(&mut requested);

        assert_eq!(requested.context_window_tokens, Some(200_000));
    }

    #[test]
    fn configured_context_window_fills_missing_client_value() {
        let input = input();
        let mut config = config(&input);
        config.context_window_tokens = Some(350_000);
        let mut requested = super::super::ModelSpec::new("model-a");

        config.configure(&mut requested);

        assert_eq!(requested.context_window_tokens, Some(350_000));
    }

    #[test]
    fn configured_default_context_fills_missing_window_and_client_value() {
        let input = input();
        let mut config = config(&input);
        config.context_window_tokens = None;
        config.default_context = Some("1m".into());
        let mut requested = super::super::ModelSpec::new("model-a");

        config.configure(&mut requested);

        // 配置窗口为空时,默认 Context 档位解析出的 token 数兜底。
        assert_eq!(requested.context_window_tokens, Some(1_000_000));
    }

    fn axis() -> ModelVariantAxis {
        ModelVariantAxis {
            context_options: vec!["200k".into(), "1m".into()],
            effort_options: vec!["low".into(), "high".into()],
            ..Default::default()
        }
    }

    #[test]
    fn variant_slug_round_trips_with_and_without_fast() {
        let axis = axis();
        let parts = ModelVariantParts {
            context: "1m".into(),
            effort: Some("low".into()),
            fast: true,
        };
        let slug = axis.bake_slug("hash", &parts);
        assert_eq!(slug, "hash-1m-low-fast");
        assert_eq!(axis.parse_slug("hash", &slug), Some(parts));
        assert_eq!(
            axis.parse_slug("hash", "hash-200k-high"),
            Some(ModelVariantParts {
                context: "200k".into(),
                effort: Some("high".into()),
                fast: false,
            })
        );
        assert_eq!(axis.parse_slug("hash", "hash-1m"), None);
        assert_eq!(axis.parse_slug("hash", "hash-1m-gone"), None);
        assert_eq!(axis.parse_slug("hash", "other-1m-low"), None);
    }

    #[test]
    fn variant_slug_omits_the_effort_segment_when_the_model_has_no_reasoning_axis() {
        let axis = ModelVariantAxis {
            context_options: vec!["200k".into(), "1m".into()],
            effort_options: Vec::new(),
            ..Default::default()
        };
        let parts = ModelVariantParts {
            context: "1m".into(),
            effort: None,
            fast: false,
        };
        assert_eq!(axis.bake_slug("hash", &parts), "hash-1m");
        assert_eq!(axis.parse_slug("hash", "hash-1m"), Some(parts));
        assert_eq!(
            axis.parse_slug("hash", "hash-1m-fast"),
            Some(ModelVariantParts {
                context: "1m".into(),
                effort: None,
                fast: true,
            })
        );
        assert_eq!(axis.parse_slug("hash", "hash-1m-low"), None);
        assert_eq!(
            axis.default_parts(),
            Some(ModelVariantParts {
                context: "200k".into(),
                effort: None,
                fast: false,
            })
        );
    }

    #[test]
    fn variant_slug_parses_the_catalog_bracket_representation() {
        let axis = axis();
        assert_eq!(
            axis.parse_slug("hash", "hash[context=1m,reasoning=low,fast=true]"),
            Some(ModelVariantParts {
                context: "1m".into(),
                effort: Some("low".into()),
                fast: true,
            })
        );
        assert_eq!(
            axis.parse_slug("hash", "hash[context=200k,reasoning=high,fast=false]"),
            Some(ModelVariantParts {
                context: "200k".into(),
                effort: Some("high".into()),
                fast: false,
            })
        );
        // 与连字符 slug 同口径:档位必须落在轴上。
        assert_eq!(
            axis.parse_slug("hash", "hash[context=2m,reasoning=low,fast=false]"),
            None
        );
        assert_eq!(
            axis.parse_slug("hash", "hash[context=1m,reasoning=gone,fast=false]"),
            None
        );
        assert_eq!(axis.parse_slug("hash", "hash[reasoning=low]"), None);

        let no_effort = ModelVariantAxis {
            context_options: vec!["200k".into(), "1m".into()],
            effort_options: Vec::new(),
            ..Default::default()
        };
        assert_eq!(
            no_effort.parse_slug("hash", "hash[context=1m,fast=false]"),
            Some(ModelVariantParts {
                context: "1m".into(),
                effort: None,
                fast: false,
            })
        );
        assert_eq!(
            no_effort.parse_slug("hash", "hash[context=1m,reasoning=low,fast=false]"),
            None
        );
    }

    #[test]
    fn default_variant_falls_back_to_the_first_axis_option() {
        let axis = ModelVariantAxis {
            context_options: vec!["272k".into(), "200k".into(), "1m".into()],
            effort_options: vec!["low".into(), "high".into()],
            ..Default::default()
        };
        assert_eq!(
            axis.default_parts(),
            Some(ModelVariantParts {
                context: "272k".into(),
                effort: Some("low".into()),
                fast: false,
            })
        );
        let single = ModelVariantAxis {
            context_options: vec!["272k".into(), "1m".into()],
            effort_options: vec!["low".into()],
            ..Default::default()
        };
        assert_eq!(
            single.default_parts(),
            Some(ModelVariantParts {
                context: "272k".into(),
                effort: Some("low".into()),
                fast: false,
            })
        );
    }

    #[test]
    fn default_variant_adopts_explicit_defaults_verbatim() {
        // 显式默认原样采用,不做轴成员校验。
        let axis = ModelVariantAxis {
            context_options: vec!["272k".into(), "200k".into()],
            effort_options: vec!["low".into(), "high".into()],
            default_context: Some("2m".into()),
            default_effort: Some("max".into()),
        };
        assert_eq!(
            axis.default_parts(),
            Some(ModelVariantParts {
                context: "2m".into(),
                effort: Some("max".into()),
                fast: false,
            })
        );
    }

    #[test]
    fn default_variant_without_a_context_axis_is_none() {
        // 无 context 轴且无显式默认时无法烘焙 slug;effort 轴空恒为 None。
        let axis = ModelVariantAxis {
            effort_options: vec!["low".into()],
            ..Default::default()
        };
        assert_eq!(axis.default_parts(), None);
        // 显式默认 Context 即便轴为空也可烘焙 slug。
        let explicit = ModelVariantAxis {
            default_context: Some("200k".into()),
            default_effort: Some("high".into()),
            ..Default::default()
        };
        assert_eq!(
            explicit.default_parts(),
            Some(ModelVariantParts {
                context: "200k".into(),
                effort: None,
                fast: false,
            })
        );
    }

    #[test]
    fn parameter_validation_normalizes_case_and_lists_valid_options() {
        let axis = axis();
        assert_eq!(axis.validate_effort("HIGH").unwrap(), "high");
        assert_eq!(axis.validate_context("200000").unwrap(), "200k");
        assert!(matches!(
            axis.validate_effort("gone"),
            Err(Error::Protocol(message)) if message.contains("low, high")
        ));
        assert!(matches!(
            axis.validate_context("2m"),
            Err(Error::Protocol(message)) if message.contains("200k, 1m")
        ));
        let no_effort = ModelVariantAxis {
            context_options: vec!["200k".into()],
            effort_options: Vec::new(),
            ..Default::default()
        };
        assert!(no_effort.validate_effort("high").is_err());
    }
}
