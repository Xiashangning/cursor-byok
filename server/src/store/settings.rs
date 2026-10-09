//! Persists application settings.
use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};

use crate::Result;

use super::{now_ms, Store};

const PORT_SETTINGS_KEY: &str = "network_ports";
const PROXY_SETTINGS_KEY: &str = "outbound_proxy";
const TAB_SETTINGS_KEY: &str = "cursor_tab";
const DESKTOP_SETTINGS_KEY: &str = "desktop_lifecycle";
const COMMIT_SETTINGS_KEY: &str = "commit_settings";
const CURSOR_TAKEOVER_ENABLED_KEY: &str = "cursor_takeover_enabled";
const DISABLED_PLUGIN_MODELS_KEY: &str = "disabled_plugin_models";
const DISABLED_PLUGIN_ACCOUNTS_KEY: &str = "disabled_plugin_accounts";
const ACCESS_TOKEN_KEY: &str = "access_token";
const PLUGIN_MODEL_OVERRIDES_KEY: &str = "plugin_model_overrides";
const APP_API_SETTINGS_KEY: &str = "app_api";

/// Embedded default system prompts for commit message generation.
pub const DEFAULT_COMMIT_PROMPT_ZH_CN: &str = include_str!("../../prompt/cursor/commit/zh-CN.md");
pub const DEFAULT_COMMIT_PROMPT_EN_US: &str = include_str!("../../prompt/cursor/commit/en-US.md");

pub const PUBLIC_TAB_SERVICE_URL: &str = "https://tab.leokun.cn";

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct PortSettings {
    pub proxy_port: u16,
    pub service_port: u16,
}

/// 本机 Agent 管理 API（/byok/app/v1）开关；鉴权复用控制 API 访问令牌。
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct AppApiSettings {
    pub enabled: bool,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProxyMode {
    #[default]
    Default,
    Custom,
}

impl ProxyMode {
    pub fn is_custom(self) -> bool {
        self == Self::Custom
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TabMode {
    #[default]
    Public,
    Direct,
    Custom,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct TabSettings {
    pub mode: TabMode,
    pub address: String,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct DesktopSettings {
    #[serde(default)]
    pub silent_start: bool,
    #[serde(default = "default_true")]
    pub show_dock_icon: bool,
}

impl Default for DesktopSettings {
    fn default() -> Self {
        Self {
            silent_start: false,
            show_dock_icon: true,
        }
    }
}

fn default_true() -> bool {
    true
}

impl TabSettings {
    pub fn service_url(&self) -> Option<&str> {
        match self.mode {
            TabMode::Public => Some(PUBLIC_TAB_SERVICE_URL),
            TabMode::Direct => None,
            TabMode::Custom => Some(&self.address),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub enum CommitPromptLocale {
    #[default]
    #[serde(rename = "zh-CN")]
    ZhCn,
    #[serde(rename = "en-US")]
    EnUs,
}

impl CommitPromptLocale {
    pub fn default_prompt(self) -> &'static str {
        match self {
            Self::ZhCn => DEFAULT_COMMIT_PROMPT_ZH_CN.trim(),
            Self::EnUs => DEFAULT_COMMIT_PROMPT_EN_US.trim(),
        }
    }
}

/// User preferences for Git commit message generation.
///
/// Empty `model_id` means 直连: forward the original Cursor RPC unchanged.
/// A non-empty value is the `model_hash` of a model configured on the Cursor
/// page, and the request is generated locally through that model.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct CommitSettings {
    #[serde(default)]
    pub parameters: crate::model::SelectionParameters,
    #[serde(default)]
    pub model_id: String,
    #[serde(default)]
    pub prompt: String,
    #[serde(default)]
    pub prompt_locale: CommitPromptLocale,
}

impl CommitSettings {
    pub fn is_direct(&self) -> bool {
        self.model_id.trim().is_empty()
    }

    pub fn effective_prompt(&self) -> &str {
        let trimmed = self.prompt.trim();
        if trimmed.is_empty() {
            self.prompt_locale.default_prompt()
        } else {
            trimmed
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct ProxySettingsInput {
    pub mode: ProxyMode,
    pub address: String,
    pub auth_enabled: bool,
    pub username: String,
    pub password: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct ProxySettings {
    pub mode: ProxyMode,
    pub address: String,
    pub auth_enabled: bool,
    pub username: String,
    pub has_password: bool,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub(crate) struct ProxySettingsSecret {
    pub mode: ProxyMode,
    pub address: String,
    pub auth_enabled: bool,
    pub username: String,
    pub password: String,
}

/// 用户对单个插件模型的手工覆盖;字段为空/None 表示恢复插件默认。
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", default)]
pub struct PluginModelOverride {
    pub display_name: Option<String>,
    pub tooltip: Option<String>,
    pub effort_options: Option<Vec<String>>,
    pub context_options: Option<Vec<String>>,
    pub max_output_tokens: Option<u64>,
    pub default_effort: Option<String>,
    pub default_context: Option<String>,
}

impl PluginModelOverride {
    /// 非 Effort 字段去空白,空值表示无覆盖;Effort 显式空轴回填五档。
    /// 全部字段未指定时删除覆盖条目。
    pub fn normalized(self) -> Self {
        let text = |value: Option<String>| {
            value
                .map(|value| value.trim().to_owned())
                .filter(|value| !value.is_empty())
        };
        let text_lower = |value: Option<String>| {
            value
                .map(|value| value.trim().to_ascii_lowercase())
                .filter(|value| !value.is_empty())
        };
        let options = |values: Option<Vec<String>>| {
            values
                .map(|values| {
                    values
                        .into_iter()
                        .map(|value| value.trim().to_ascii_lowercase())
                        .filter(|value| !value.is_empty())
                        .collect::<Vec<_>>()
                })
                .filter(|values| !values.is_empty())
        };
        let (effort_options, default_effort) = crate::model::normalize_plugin_effort_override(
            self.effort_options,
            self.default_effort,
        );
        Self {
            display_name: text(self.display_name),
            tooltip: text(self.tooltip),
            effort_options,
            context_options: options(self.context_options),
            max_output_tokens: self.max_output_tokens.filter(|tokens| *tokens > 0),
            default_effort,
            default_context: text_lower(self.default_context),
        }
    }
}

/// Reads the persisted outbound proxy row, falling back to "no proxy" when it
/// no longer parses.
///
/// `mode` is a closed enum whose stored wire value has already changed once, so
/// a row written by an older build can be unreadable by this one. Propagating
/// that error would be unrecoverable rather than merely noisy: every outbound
/// client is built from this value, and `set_proxy_settings` reads the row
/// before it writes, so the settings page could neither load nor replace the
/// row that broke it.
fn read_proxy_settings(value: &str) -> ProxySettingsSecret {
    serde_json::from_str(value).unwrap_or_else(|error| {
        tracing::warn!(%error, "ignoring unreadable outbound proxy settings");
        ProxySettingsSecret::default()
    })
}

impl From<ProxySettingsSecret> for ProxySettings {
    fn from(settings: ProxySettingsSecret) -> Self {
        Self {
            mode: settings.mode,
            address: settings.address,
            auth_enabled: settings.auth_enabled,
            username: settings.username,
            has_password: !settings.password.is_empty(),
        }
    }
}

impl Store {
    /// Reads one `service_settings` row as JSON, or `None` when unset.
    async fn read_setting(&self, key: &str) -> Result<Option<String>> {
        Ok(sqlx::query_scalar::<_, String>(
            "SELECT value_json FROM service_settings WHERE setting_key = ?",
        )
        .bind(key)
        .fetch_optional(&self.pool)
        .await?)
    }

    /// Inserts or replaces one `service_settings` row inside the caller's
    /// transaction, on the executor the transaction (or pool) provides.
    async fn write_setting(
        executor: impl sqlx::Executor<'_, Database = sqlx::Sqlite>,
        key: &str,
        value_json: &str,
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO service_settings(setting_key, value_json, updated_at_ms) VALUES (?, ?, ?) ON CONFLICT(setting_key) DO UPDATE SET value_json = excluded.value_json, updated_at_ms = excluded.updated_at_ms",
        )
        .bind(key)
        .bind(value_json)
        .bind(now_ms())
        .execute(executor)
        .await?;
        Ok(())
    }

    /// Writes one `service_settings` row under the store-wide write lock.
    async fn store_setting(&self, key: &str, value_json: &str) -> Result<()> {
        let _write = self.writes.lock().await;
        Self::write_setting(&self.pool, key, value_json).await
    }

    pub async fn detailed_logging(&self) -> Result<bool> {
        let value = self
            .read_setting("llm_detailed_logging")
            .await?
            .ok_or(sqlx::Error::RowNotFound)?;
        Ok(serde_json::from_str(&value)?)
    }

    pub async fn set_detailed_logging(&self, enabled: bool) -> Result<()> {
        self.store_setting("llm_detailed_logging", &serde_json::to_string(&enabled)?)
            .await
    }

    pub(crate) async fn cursor_takeover_enabled(&self) -> Result<bool> {
        let value = self.read_setting(CURSOR_TAKEOVER_ENABLED_KEY).await?;
        value
            .map(|value| serde_json::from_str(&value).map_err(Into::into))
            .unwrap_or(Ok(true))
    }

    pub(crate) async fn set_cursor_takeover_enabled(&self, enabled: bool) -> Result<()> {
        self.store_setting(
            CURSOR_TAKEOVER_ENABLED_KEY,
            &serde_json::to_string(&enabled)?,
        )
        .await
    }

    pub(crate) async fn proxy_settings_secret(&self) -> Result<ProxySettingsSecret> {
        let value = self.read_setting(PROXY_SETTINGS_KEY).await?;
        Ok(value
            .as_deref()
            .map_or_else(ProxySettingsSecret::default, read_proxy_settings))
    }

    pub async fn proxy_settings(&self) -> Result<ProxySettings> {
        Ok(self.proxy_settings_secret().await?.into())
    }

    /// 读-改-写整体串行化:旧密码在写锁内读取,避免并发保存把对方刚写入的
    /// 密码覆盖掉(对齐 set_service_port)。
    pub async fn set_proxy_settings(&self, input: ProxySettingsInput) -> Result<ProxySettings> {
        let address = input.address.trim().to_owned();
        if input.mode.is_custom() {
            let parsed = url::Url::parse(&address)
                .map_err(|error| crate::Error::Config(format!("invalid proxy address: {error}")))?;
            if !matches!(parsed.scheme(), "http" | "https" | "socks5" | "socks5h") {
                return Err(crate::Error::Config(
                    "proxy address must use http, https, socks5, or socks5h".into(),
                ));
            }
            reqwest::Proxy::all(&address)?;
        }
        let _write = self.writes.lock().await;
        let existing = self.proxy_settings_secret().await?;
        let password = if input.auth_enabled {
            input
                .password
                .filter(|password| !password.is_empty())
                .unwrap_or(existing.password)
        } else {
            String::new()
        };
        let settings = ProxySettingsSecret {
            mode: input.mode,
            address,
            auth_enabled: input.auth_enabled,
            username: input.username.trim().to_owned(),
            password,
        };
        let value_json = serde_json::to_string(&settings)?;
        Self::write_setting(&self.pool, PROXY_SETTINGS_KEY, &value_json).await?;
        Ok(settings.into())
    }

    pub async fn tab_settings(&self) -> Result<TabSettings> {
        let value = self.read_setting(TAB_SETTINGS_KEY).await?;
        value
            .map(|value| serde_json::from_str(&value).map_err(Into::into))
            .unwrap_or_else(|| Ok(TabSettings::default()))
    }

    pub async fn set_tab_settings(&self, mut settings: TabSettings) -> Result<TabSettings> {
        settings.address = settings.address.trim().trim_end_matches('/').to_owned();
        if settings.mode == TabMode::Custom {
            let parsed = url::Url::parse(&settings.address).map_err(|error| {
                crate::Error::Config(format!("invalid TAB service address: {error}"))
            })?;
            if !matches!(parsed.scheme(), "http" | "https") {
                return Err(crate::Error::Config(
                    "TAB service address must use http or https".into(),
                ));
            }
            if parsed.host_str().is_none()
                || parsed.query().is_some()
                || parsed.fragment().is_some()
            {
                return Err(crate::Error::Config(
                    "TAB service address must be a base URL without a query or fragment".into(),
                ));
            }
        }
        self.store_setting(TAB_SETTINGS_KEY, &serde_json::to_string(&settings)?)
            .await?;
        Ok(settings)
    }

    pub async fn port_settings(&self) -> Result<PortSettings> {
        let value = self.read_setting(PORT_SETTINGS_KEY).await?;
        value
            .map(|value| serde_json::from_str(&value).map_err(Into::into))
            .unwrap_or_else(|| Ok(PortSettings::default()))
    }

    pub async fn set_port_settings(&self, settings: PortSettings) -> Result<()> {
        self.store_setting(PORT_SETTINGS_KEY, &serde_json::to_string(&settings)?)
            .await
    }

    /// 读-改-写整体串行化(对齐 set_plugin_model_override),避免并发保存互相覆盖。
    pub async fn set_service_port(&self, port: u16) -> Result<()> {
        let _write = self.writes.lock().await;
        let mut settings = self.port_settings().await?;
        settings.service_port = port;
        self.write_port_settings(&settings).await
    }

    pub async fn set_proxy_port(&self, port: u16) -> Result<()> {
        let _write = self.writes.lock().await;
        let mut settings = self.port_settings().await?;
        settings.proxy_port = port;
        self.write_port_settings(&settings).await
    }

    /// Caller must hold the write lock.
    async fn write_port_settings(&self, settings: &PortSettings) -> Result<()> {
        Self::write_setting(
            &self.pool,
            PORT_SETTINGS_KEY,
            &serde_json::to_string(settings)?,
        )
        .await
    }

    pub async fn desktop_settings(&self) -> Result<DesktopSettings> {
        let value = self.read_setting(DESKTOP_SETTINGS_KEY).await?;
        value
            .map(|value| serde_json::from_str(&value).map_err(Into::into))
            .unwrap_or_else(|| Ok(DesktopSettings::default()))
    }

    pub async fn set_desktop_settings(&self, settings: DesktopSettings) -> Result<()> {
        self.store_setting(DESKTOP_SETTINGS_KEY, &serde_json::to_string(&settings)?)
            .await
    }

    pub async fn commit_settings(&self) -> Result<CommitSettings> {
        let value = self.read_setting(COMMIT_SETTINGS_KEY).await?;
        value
            .map(|value| serde_json::from_str(&value).map_err(Into::into))
            .unwrap_or_else(|| Ok(CommitSettings::default()))
    }

    pub async fn set_commit_settings(&self, settings: CommitSettings) -> Result<CommitSettings> {
        self.set_commit_settings_checked(settings, &[]).await
    }

    pub(crate) async fn set_commit_settings_checked(
        &self,
        mut settings: CommitSettings,
        plugins: &[crate::plugin::PluginModelDescriptor],
    ) -> Result<CommitSettings> {
        if !settings.is_direct() {
            let directory = crate::model::ModelDirectory::new(&self.models().await?, plugins)?;
            match directory.resolve(settings.model_id.trim()) {
                crate::model::Resolution::Matched { id, parts } => {
                    settings.model_id = id;
                    if let Some(parts) = parts {
                        settings.parameters = parts.into();
                    }
                }
                crate::model::Resolution::Ambiguous { candidates } => {
                    return Err(crate::Error::Config(format!(
                        "commit model is ambiguous; use one of: {}",
                        candidates.join(", ")
                    )))
                }
                crate::model::Resolution::NotFound => {
                    return Err(crate::Error::Config(
                        "commit model is not configured".into(),
                    ))
                }
            }
        } else {
            settings.parameters = Default::default();
        }
        let settings = CommitSettings {
            model_id: settings.model_id.trim().to_owned(),
            parameters: settings.parameters,
            prompt: settings.prompt.trim().to_owned(),
            prompt_locale: settings.prompt_locale,
        };
        self.store_setting(COMMIT_SETTINGS_KEY, &serde_json::to_string(&settings)?)
            .await?;
        Ok(settings)
    }

    pub async fn disabled_plugin_models(&self) -> Result<HashSet<String>> {
        let value = self.read_setting(DISABLED_PLUGIN_MODELS_KEY).await?;
        value
            .map(|value| serde_json::from_str(&value).map_err(Into::into))
            .unwrap_or_else(|| Ok(HashSet::new()))
    }

    pub async fn set_disabled_plugin_models(&self, model_ids: &HashSet<String>) -> Result<()> {
        self.store_setting(
            DISABLED_PLUGIN_MODELS_KEY,
            &serde_json::to_string(model_ids)?,
        )
        .await
    }

    pub async fn disabled_plugin_accounts(&self) -> Result<HashSet<String>> {
        let value = self.read_setting(DISABLED_PLUGIN_ACCOUNTS_KEY).await?;
        value
            .map(|value| serde_json::from_str(&value).map_err(Into::into))
            .unwrap_or_else(|| Ok(HashSet::new()))
    }

    pub async fn set_disabled_plugin_accounts(&self, account_ids: &HashSet<String>) -> Result<()> {
        self.store_setting(
            DISABLED_PLUGIN_ACCOUNTS_KEY,
            &serde_json::to_string(account_ids)?,
        )
        .await
    }

    /// 控制 API 的访问令牌;未设置过则为 None,由控制层在启动时生成并写回。
    pub async fn access_token(&self) -> Result<Option<String>> {
        let value = self.read_setting(ACCESS_TOKEN_KEY).await?;
        value
            .map(|value| serde_json::from_str(&value).map_err(Into::into))
            .transpose()
    }

    pub async fn set_access_token(&self, token: &str) -> Result<()> {
        self.store_setting(ACCESS_TOKEN_KEY, &serde_json::to_string(token)?)
            .await
    }

    pub async fn app_api_settings(&self) -> Result<AppApiSettings> {
        let value = self.read_setting(APP_API_SETTINGS_KEY).await?;
        value
            .map(|value| serde_json::from_str(&value).map_err(Into::into))
            .unwrap_or_else(|| Ok(AppApiSettings::default()))
    }

    pub async fn set_app_api_settings(&self, settings: AppApiSettings) -> Result<AppApiSettings> {
        self.store_setting(APP_API_SETTINGS_KEY, &serde_json::to_string(&settings)?)
            .await?;
        Ok(settings)
    }

    pub async fn plugin_model_overrides(&self) -> Result<HashMap<String, PluginModelOverride>> {
        let value = self.read_setting(PLUGIN_MODEL_OVERRIDES_KEY).await?;
        value
            .map(|value| serde_json::from_str(&value).map_err(Into::into))
            .unwrap_or_else(|| Ok(HashMap::new()))
    }

    /// 覆盖先归一再写入;全空时删除该模型的条目(即恢复插件默认)。
    /// `id` 为模型的统一 8 位 ID。
    pub async fn set_plugin_model_override(
        &self,
        id: &str,
        over: PluginModelOverride,
    ) -> Result<()> {
        // 读-改-写整体串行化,避免并发保存互相覆盖。
        let _write = self.writes.lock().await;
        let mut overrides = self.plugin_model_overrides().await?;
        let over = over.normalized();
        if over == PluginModelOverride::default() {
            overrides.remove(id);
        } else {
            overrides.insert(id.to_owned(), over);
        }
        Self::write_setting(
            &self.pool,
            PLUGIN_MODEL_OVERRIDES_KEY,
            &serde_json::to_string(&overrides)?,
        )
        .await
    }

    /// 删除某插件在数据库中的全部模型设置:模型开关、模型覆盖,以及这些
    /// 账号记录的停用标记。`model_ids` 为该插件当前全部模型的统一 ID;
    /// 账号记录 ID 由调用方从插件资源读出。
    pub async fn clear_plugin_settings(
        &self,
        model_ids: &[String],
        account_ids: &[String],
    ) -> Result<()> {
        let model_ids: HashSet<_> = model_ids.iter().collect();
        let account_ids: HashSet<_> = account_ids.iter().collect();
        let _write = self.writes.lock().await;
        let disabled_models = self
            .disabled_plugin_models()
            .await?
            .into_iter()
            .filter(|id| !model_ids.contains(id))
            .collect::<HashSet<_>>();
        let overrides = self
            .plugin_model_overrides()
            .await?
            .into_iter()
            .filter(|(id, _)| !model_ids.contains(id))
            .collect::<HashMap<_, _>>();
        let disabled_accounts = self
            .disabled_plugin_accounts()
            .await?
            .into_iter()
            .filter(|id| !account_ids.contains(id))
            .collect::<HashSet<_>>();
        let rows = [
            (
                DISABLED_PLUGIN_MODELS_KEY,
                serde_json::to_string(&disabled_models)?,
            ),
            (
                PLUGIN_MODEL_OVERRIDES_KEY,
                serde_json::to_string(&overrides)?,
            ),
            (
                DISABLED_PLUGIN_ACCOUNTS_KEY,
                serde_json::to_string(&disabled_accounts)?,
            ),
        ];
        // 三个键整体替换,避免只清掉其中一部分。
        let mut transaction = self.pool.begin().await?;
        for (key, value_json) in &rows {
            Self::write_setting(&mut *transaction, key, value_json).await?;
        }
        transaction.commit().await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{
        read_proxy_settings, AppApiSettings, PluginModelOverride, ProxyMode, ProxySettingsInput,
        ProxySettingsSecret, Store, PROXY_SETTINGS_KEY,
    };

    /// The `outbound_proxy` row exactly as builds before the `system` -> `default`
    /// rename wrote it.
    const LEGACY_PROXY_ROW: &str =
        r#"{"mode":"system","address":"","auth_enabled":false,"username":"","password":""}"#;

    #[test]
    fn default_proxy_mode_uses_the_default_wire_value() {
        assert_eq!(ProxyMode::default(), ProxyMode::Default);
        assert_eq!(
            serde_json::to_string(&ProxyMode::default()).unwrap(),
            "\"default\""
        );
        assert_eq!(
            serde_json::from_str::<ProxyMode>("\"default\"").unwrap(),
            ProxyMode::Default
        );
        assert!(serde_json::from_str::<ProxyMode>("\"system\"").is_err());
    }

    #[tokio::test]
    async fn plugin_model_override_round_trips_and_drops_empty_entries() {
        let store = Store::connect("sqlite::memory:").await.unwrap();
        let model_id = "07929241";
        assert!(store.plugin_model_overrides().await.unwrap().is_empty());

        store
            .set_plugin_model_override(
                model_id,
                PluginModelOverride {
                    display_name: Some("  Kimi K2 ".into()),
                    tooltip: Some("   ".into()),
                    effort_options: Some(vec!["low".into(), " ".into()]),
                    context_options: Some(Vec::new()),
                    max_output_tokens: Some(0),
                    default_effort: Some(" HIGH ".into()),
                    default_context: Some(" 1M ".into()),
                },
            )
            .await
            .unwrap();
        let overrides = store.plugin_model_overrides().await.unwrap();
        // 轴归一后为 ["low"]:"high" 在白名单但不在轴上,默认档位锚定轴首项 "low"
        // (与内置模型保存同规则:默认值有效则保留,否则取最终轴首项)。
        assert_eq!(
            overrides.get(model_id),
            Some(&PluginModelOverride {
                display_name: Some("Kimi K2".into()),
                tooltip: None,
                effort_options: Some(vec!["low".into()]),
                context_options: None,
                max_output_tokens: None,
                default_effort: Some("low".into()),
                default_context: Some("1m".into()),
            })
        );

        store
            .set_plugin_model_override(model_id, PluginModelOverride::default())
            .await
            .unwrap();
        assert!(store.plugin_model_overrides().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn commit_locale_and_integration_preference_round_trip_without_losing_plugin_settings() {
        use super::{CommitPromptLocale, CommitSettings};
        let store = Store::connect("sqlite::memory:").await.unwrap();
        assert!(store.cursor_takeover_enabled().await.unwrap());
        let id = "07929241";
        let over = PluginModelOverride {
            display_name: Some("Custom".into()),
            ..Default::default()
        };
        store
            .set_plugin_model_override(id, over.clone())
            .await
            .unwrap();
        store
            .set_disabled_plugin_models(&std::collections::HashSet::from([id.to_owned()]))
            .await
            .unwrap();
        let settings = CommitSettings {
            model_id: String::new(),
            parameters: Default::default(),
            prompt: String::new(),
            prompt_locale: CommitPromptLocale::EnUs,
        };
        store.set_commit_settings(settings.clone()).await.unwrap();
        store.set_cursor_takeover_enabled(false).await.unwrap();
        assert_eq!(store.commit_settings().await.unwrap(), settings);
        assert_eq!(
            settings.effective_prompt(),
            super::DEFAULT_COMMIT_PROMPT_EN_US.trim()
        );
        assert!(!store.cursor_takeover_enabled().await.unwrap());
        assert!(store.disabled_plugin_models().await.unwrap().contains(id));
        assert_eq!(
            store.plugin_model_overrides().await.unwrap().get(id),
            Some(&over)
        );
        store
            .set_disabled_plugin_models(&std::collections::HashSet::new())
            .await
            .unwrap();
        assert!(store.disabled_plugin_models().await.unwrap().is_empty());
        for locale in [CommitPromptLocale::EnUs, CommitPromptLocale::ZhCn] {
            let custom = CommitSettings {
                prompt: " custom ".into(),
                prompt_locale: locale,
                ..Default::default()
            };
            assert_eq!(
                store
                    .set_commit_settings(custom)
                    .await
                    .unwrap()
                    .effective_prompt(),
                "custom"
            );
        }
    }

    /// 清空插件只删除该插件的模型开关、模型覆盖与账号停用标记,其他插件保持原样。
    #[tokio::test]
    async fn clearing_plugin_settings_keeps_other_plugins() {
        use std::collections::HashSet;
        let store = Store::connect("sqlite::memory:").await.unwrap();
        let cleared = "07929241";
        let kept = "7b3a6f3a";
        for id in [cleared, kept] {
            store
                .set_plugin_model_override(
                    id,
                    PluginModelOverride {
                        display_name: Some("Custom".into()),
                        ..Default::default()
                    },
                )
                .await
                .unwrap();
            store
                .set_disabled_plugin_models(&std::collections::HashSet::from([id.to_owned()]))
                .await
                .unwrap();
        }
        store
            .set_disabled_plugin_accounts(&HashSet::from([
                "account-a".to_owned(),
                "account-b".to_owned(),
            ]))
            .await
            .unwrap();

        store
            .clear_plugin_settings(&[cleared.to_owned()], &["account-a".to_owned()])
            .await
            .unwrap();

        assert_eq!(
            store.disabled_plugin_models().await.unwrap(),
            HashSet::from([kept.to_owned()])
        );
        assert_eq!(
            store
                .plugin_model_overrides()
                .await
                .unwrap()
                .into_keys()
                .collect::<Vec<_>>(),
            vec![kept]
        );
        assert_eq!(
            store.disabled_plugin_accounts().await.unwrap(),
            HashSet::from(["account-b".to_owned()])
        );
    }

    #[test]
    fn an_unreadable_proxy_row_reads_as_no_proxy() {
        assert!(serde_json::from_str::<ProxySettingsSecret>(LEGACY_PROXY_ROW).is_err());

        let settings = read_proxy_settings(LEGACY_PROXY_ROW);
        assert_eq!(settings.mode, ProxyMode::Default);
        assert!(settings.address.is_empty());
        assert!(!settings.auth_enabled);
    }

    #[tokio::test]
    async fn a_proxy_row_from_an_older_build_stays_replaceable() {
        let directory = tempfile::tempdir().unwrap();
        let url = format!("sqlite://{}", directory.path().join("test.db").display());
        let store = Store::connect(&url).await.unwrap();
        sqlx::query(
            "INSERT INTO service_settings(setting_key, value_json, updated_at_ms) VALUES (?, ?, 0)",
        )
        .bind(PROXY_SETTINGS_KEY)
        .bind(LEGACY_PROXY_ROW)
        .execute(store.pool())
        .await
        .unwrap();

        // Reading must not fail: every outbound client is built from this value.
        assert_eq!(
            store.proxy_settings().await.unwrap().mode,
            ProxyMode::Default
        );

        // And the settings page must be able to overwrite the row that broke it.
        let saved = store
            .set_proxy_settings(ProxySettingsInput {
                mode: ProxyMode::Custom,
                address: "http://127.0.0.1:7890".into(),
                auth_enabled: false,
                username: String::new(),
                password: None,
            })
            .await
            .unwrap();
        assert_eq!(saved.mode, ProxyMode::Custom);
        assert_eq!(saved.address, "http://127.0.0.1:7890");
    }

    #[tokio::test]
    async fn app_api_settings_default_to_disabled_and_persist() {
        let directory = tempfile::tempdir().unwrap();
        let url = format!("sqlite://{}", directory.path().join("test.db").display());
        let store = Store::connect(&url).await.unwrap();

        assert_eq!(
            store.app_api_settings().await.unwrap(),
            AppApiSettings { enabled: false }
        );
        let enabled = AppApiSettings { enabled: true };
        assert_eq!(store.set_app_api_settings(enabled).await.unwrap(), enabled);
        // 重新连接后读到同一个开关。
        assert_eq!(
            Store::connect(&url)
                .await
                .unwrap()
                .app_api_settings()
                .await
                .unwrap(),
            enabled
        );
    }

    /// Two interleaved proxy saves must not lose the password. Save 1 changes
    /// the address keeping the stored password; save 2 sets a new password.
    /// Whichever save lands last must have read the other's write, so the new
    /// password always survives. (Runs many truly concurrent iterations so the
    /// stale-read window is hit reliably if the read ever moves outside the
    /// write lock.)
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn concurrent_proxy_saves_do_not_lose_the_password() {
        let input = |address: &str, password: Option<&str>| ProxySettingsInput {
            mode: ProxyMode::Custom,
            address: address.into(),
            auth_enabled: true,
            username: "alice".into(),
            password: password.map(Into::into),
        };
        for _ in 0..20 {
            let store = Store::connect("sqlite::memory:").await.unwrap();
            store
                .set_proxy_settings(input("http://start:0", None))
                .await
                .unwrap();

            let first = store.clone();
            let second = store.clone();
            let (save_a, save_b) = tokio::join!(
                tokio::spawn(async move {
                    first
                        .set_proxy_settings(input("http://a:1", None))
                        .await
                        .unwrap()
                }),
                tokio::spawn(async move {
                    second
                        .set_proxy_settings(input("http://b:2", Some("new")))
                        .await
                        .unwrap()
                }),
            );
            save_a.unwrap();
            save_b.unwrap();

            // The new password must survive; a save that read the state before
            // the other's write would clobber it back to the empty password.
            let secret = store.proxy_settings_secret().await.unwrap();
            assert_eq!(secret.password, "new");
            assert!(secret.auth_enabled);
            assert!(
                matches!(secret.address.as_str(), "http://a:1" | "http://b:2"),
                "unexpected address: {}",
                secret.address
            );
        }
    }
}
