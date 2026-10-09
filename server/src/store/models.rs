//! Persists model and provider configuration.
use std::{collections::HashSet, str::FromStr};

use sqlx::{Row, Sqlite, Transaction};

use crate::{
    model::{model_hash, normalize_model_input, ModelConfig, ModelConfigInput, ModelType},
    Error, Result,
};

use super::{now_ms, Store};

const MODEL_COLUMNS: &str = r#"
    model_hash, sort_order, display_name, group_name, model_type, base_url, use_full_url, api_key, tooltip_data,
    model_id, default_context, default_effort, effort_options_json, context_options_json, openai_endpoint, openai_extra_params_enabled,
    openai_extra_params_json, custom_headers_enabled, custom_headers_json,
    anthropic_extra_params_enabled, anthropic_extra_params_json, context_window_tokens,
    max_completion_tokens, anthropic_max_tokens,
    thinking_budget_tokens, created_at_ms, updated_at_ms
"#;

impl Store {
    pub async fn models(&self) -> Result<Vec<ModelConfig>> {
        let query =
            format!("SELECT {MODEL_COLUMNS} FROM model_configs ORDER BY sort_order, display_name");
        sqlx::query(&query)
            .fetch_all(&self.pool)
            .await?
            .into_iter()
            .map(model_from_row)
            .collect()
    }

    pub async fn model(&self, hash: &str) -> Result<Option<ModelConfig>> {
        let query = format!("SELECT {MODEL_COLUMNS} FROM model_configs WHERE model_hash = ?");
        sqlx::query(&query)
            .bind(hash)
            .fetch_optional(&self.pool)
            .await?
            .map(model_from_row)
            .transpose()
    }

    pub async fn create_model(&self, input: &ModelConfigInput) -> Result<ModelConfig> {
        let mut models = self.create_models(std::slice::from_ref(input)).await?;
        Ok(models.remove(0))
    }

    pub async fn create_models(&self, inputs: &[ModelConfigInput]) -> Result<Vec<ModelConfig>> {
        self.create_models_checked(inputs, &[]).await
    }

    pub(crate) async fn create_models_checked(
        &self,
        inputs: &[ModelConfigInput],
        plugins: &[crate::plugin::PluginModelDescriptor],
    ) -> Result<Vec<ModelConfig>> {
        if inputs.is_empty() {
            return Err(Error::Config("at least one model is required".into()));
        }
        let mut normalized = Vec::with_capacity(inputs.len());
        let mut hashes = HashSet::with_capacity(inputs.len());
        for input in inputs {
            let input = normalize_model_input(input)?;
            let hash = if input.model_id.is_empty() {
                let mut identity = input.clone();
                identity.model_id = uuid::Uuid::new_v4().to_string();
                model_hash(&identity)?
            } else {
                model_hash(&input)?
            };
            if !hashes.insert(hash.clone()) {
                return Err(Error::Config("model configurations must be unique".into()));
            }
            normalized.push((hash, input));
        }
        let now = now_ms();
        let _write = self.writes.lock().await;
        let directory = crate::model::ModelDirectory::new(&self.models().await?, plugins)?;
        for (hash, _) in &normalized {
            if directory.contains(hash) {
                return Err(Error::Config(format!(
                    "model ID conflict: '{hash}' is already configured"
                )));
            }
        }
        let mut transaction = self.pool.begin().await?;
        for (hash, input) in &normalized {
            insert_model(&mut transaction, hash, input, now).await?;
        }
        transaction.commit().await?;

        let mut saved = Vec::with_capacity(normalized.len());
        for (hash, _) in normalized {
            saved.push(self.model(&hash).await?.expect("inserted model must exist"));
        }
        Ok(saved)
    }

    pub async fn update_model(
        &self,
        current_hash: &str,
        input: &ModelConfigInput,
    ) -> Result<ModelConfig> {
        self.update_model_checked(current_hash, input, &[]).await
    }

    pub(crate) async fn update_model_checked(
        &self,
        current_hash: &str,
        input: &ModelConfigInput,
        plugins: &[crate::plugin::PluginModelDescriptor],
    ) -> Result<ModelConfig> {
        let _write = self.writes.lock().await;
        let current = self
            .model(current_hash)
            .await?
            .ok_or_else(|| Error::RunNotFound(format!("model {current_hash}")))?;
        let input = normalize_model_input(input)?;
        let next_hash = next_model_hash(&input, &current)?;
        let now = now_ms();
        let others: Vec<_> = self
            .models()
            .await?
            .into_iter()
            .filter(|model| model.model_hash != current_hash)
            .collect();
        let directory = crate::model::ModelDirectory::new(&others, plugins)?;
        if directory.contains(&next_hash) {
            return Err(Error::Config(format!(
                "model ID conflict: '{next_hash}' is already configured"
            )));
        }
        let mut transaction = self.pool.begin().await?;
        update_model_row(&mut transaction, current_hash, &next_hash, &input, now).await?;
        transaction.commit().await?;
        Ok(self
            .model(&next_hash)
            .await?
            .expect("updated model must exist"))
    }

    /// 分组设置整批更新:每条编辑只携带用户修改的字段,存储层在写入锁内读取
    /// 当前配置、合并、归一并统一校验,全部条目在单个事务中生效;任一条目
    /// 失败即整体回滚。占位符密钥视为「未修改」,从现有配置回填。
    pub(crate) async fn update_models_group(
        &self,
        edits: &[crate::model::ModelGroupEdit],
        plugins: &[crate::plugin::PluginModelDescriptor],
    ) -> Result<Vec<ModelConfig>> {
        if edits.is_empty() {
            return Err(Error::Config("at least one model edit is required".into()));
        }
        let _write = self.writes.lock().await;
        let mut transaction = self.pool.begin().await?;
        let query =
            format!("SELECT {MODEL_COLUMNS} FROM model_configs ORDER BY sort_order, display_name");
        let all = sqlx::query(&query)
            .fetch_all(&mut *transaction)
            .await?
            .into_iter()
            .map(model_from_row)
            .collect::<Result<Vec<_>>>()?;
        let mut updates: Vec<(String, String, ModelConfigInput)> = Vec::with_capacity(edits.len());
        let mut batch_hashes = HashSet::with_capacity(edits.len());
        for edit in edits {
            let current = all
                .iter()
                .find(|model| model.model_hash == edit.model_hash)
                .ok_or_else(|| Error::RunNotFound(format!("model {}", edit.model_hash)))?;
            let mut input = current.clone().into_input();
            if let Some(group_name) = &edit.group_name {
                input.group_name = Some(group_name.clone());
            }
            if let Some(base_url) = &edit.base_url {
                input.base_url = base_url.clone();
            }
            if let Some(api_key) = &edit.api_key {
                if api_key != crate::model::REDACTED_SECRET {
                    input.api_key = api_key.clone();
                }
            }
            let input = normalize_model_input(&input)?;
            let next_hash = next_model_hash(&input, current)?;
            if !batch_hashes.insert(next_hash.clone()) {
                return Err(Error::Config(format!(
                    "model ID conflict: '{next_hash}' is already configured"
                )));
            }
            updates.push((current.model_hash.clone(), next_hash, input));
        }
        // 目录包含全量现配置:批内条目的新身份若与他人现有身份冲突,整批失败。
        let directory = crate::model::ModelDirectory::new(&all, plugins)?;
        for (current_hash, next_hash, _) in &updates {
            if next_hash != current_hash && directory.contains(next_hash) {
                return Err(Error::Config(format!(
                    "model ID conflict: '{next_hash}' is already configured"
                )));
            }
        }
        let now = now_ms();
        for (current_hash, next_hash, input) in &updates {
            update_model_row(&mut transaction, current_hash, next_hash, input, now).await?;
        }
        transaction.commit().await?;
        self.models().await
    }

    pub async fn delete_model(&self, hash: &str) -> Result<()> {
        let _write = self.writes.lock().await;
        let mut transaction = self.pool.begin().await?;
        sqlx::query("UPDATE llm_calls SET model_hash = NULL WHERE model_hash = ?")
            .bind(hash)
            .execute(&mut *transaction)
            .await?;
        let result = sqlx::query("DELETE FROM model_configs WHERE model_hash = ?")
            .bind(hash)
            .execute(&mut *transaction)
            .await?;
        if result.rows_affected() != 1 {
            return Err(Error::RunNotFound(format!("model {hash}")));
        }
        transaction.commit().await?;
        Ok(())
    }

    pub async fn reorder_models(&self, model_hashes: &[String]) -> Result<Vec<ModelConfig>> {
        let current = self.models().await?;
        let current_hashes = current
            .iter()
            .map(|model| model.model_hash.as_str())
            .collect::<HashSet<_>>();
        let requested_hashes = model_hashes
            .iter()
            .map(String::as_str)
            .collect::<HashSet<_>>();
        if model_hashes.len() != current.len()
            || requested_hashes.len() != current.len()
            || requested_hashes != current_hashes
        {
            return Err(Error::Config(
                "model configuration changed; refresh and try sorting again".into(),
            ));
        }

        let now = now_ms();
        let _write = self.writes.lock().await;
        let mut transaction = self.pool.begin().await?;
        for (index, hash) in model_hashes.iter().enumerate() {
            sqlx::query(
                "UPDATE model_configs SET sort_order = ?, updated_at_ms = ? WHERE model_hash = ?",
            )
            .bind(i64::try_from(index + 1).expect("model order fits in i64"))
            .bind(now)
            .bind(hash)
            .execute(&mut *transaction)
            .await?;
        }
        transaction.commit().await?;
        self.models().await
    }
}

fn next_model_hash(input: &ModelConfigInput, current: &ModelConfig) -> Result<String> {
    if !input.model_id.is_empty() {
        return model_hash(input);
    }
    if current.model_id.is_empty() {
        return Ok(current.model_hash.clone());
    }
    let mut identity = input.clone();
    identity.model_id = uuid::Uuid::new_v4().to_string();
    model_hash(&identity)
}

/// 单行更新;身份变化时同步解除旧哈希上的调用关联。
async fn update_model_row(
    transaction: &mut Transaction<'_, Sqlite>,
    current_hash: &str,
    next_hash: &str,
    input: &ModelConfigInput,
    now: i64,
) -> Result<()> {
    if next_hash != current_hash {
        sqlx::query("UPDATE llm_calls SET model_hash = NULL WHERE model_hash = ?")
            .bind(current_hash)
            .execute(&mut **transaction)
            .await?;
    }
    let result = sqlx::query(
        r#"UPDATE model_configs SET
            model_hash = ?, sort_order = ?, display_name = ?, group_name = ?, model_type = ?, base_url = ?,
            use_full_url = ?, api_key = ?, tooltip_data = ?, model_id = ?, default_context = ?, default_effort = ?,
            effort_options_json = ?, context_options_json = ?, openai_endpoint = ?,
            openai_extra_params_enabled = ?, openai_extra_params_json = ?,
            custom_headers_enabled = ?, custom_headers_json = ?,
            anthropic_extra_params_enabled = ?, anthropic_extra_params_json = ?,
            context_window_tokens = ?, max_completion_tokens = ?, anthropic_max_tokens = ?,
            thinking_budget_tokens = ?, updated_at_ms = ?
        WHERE model_hash = ?"#,
    )
    .bind(next_hash)
    .bind(input.sort_order)
    .bind(&input.display_name)
    .bind(&input.group_name)
    .bind(input.model_type.as_str())
    .bind(&input.base_url)
    .bind(input.use_full_url)
    .bind(&input.api_key)
    .bind(&input.tooltip_data)
    .bind(&input.model_id)
    .bind(&input.default_context)
    .bind(&input.default_effort)
    .bind(serde_json::to_string(&input.effort_options)?)
    .bind(serde_json::to_string(&input.context_options)?)
    .bind(&input.openai_endpoint)
    .bind(input.openai_extra_params_enabled)
    .bind(serde_json::to_string(&input.openai_extra_params)?)
    .bind(input.custom_headers_enabled)
    .bind(serde_json::to_string(&input.custom_headers)?)
    .bind(input.anthropic_extra_params_enabled)
    .bind(serde_json::to_string(&input.anthropic_extra_params)?)
    .bind(input.context_window_tokens.map(to_i64).transpose()?)
    .bind(input.max_completion_tokens.map(to_i64).transpose()?)
    .bind(input.anthropic_max_tokens.map(to_i64).transpose()?)
    .bind(input.thinking_budget_tokens.map(to_i64).transpose()?)
    .bind(now)
    .bind(current_hash)
    .execute(&mut **transaction)
    .await?;
    if result.rows_affected() != 1 {
        return Err(Error::RunNotFound(format!("model {current_hash}")));
    }
    Ok(())
}

async fn insert_model(
    transaction: &mut Transaction<'_, Sqlite>,
    hash: &str,
    input: &ModelConfigInput,
    now: i64,
) -> Result<()> {
    sqlx::query(
        r#"INSERT INTO model_configs(
            model_hash, sort_order, display_name, group_name, model_type, base_url, use_full_url, api_key, tooltip_data,
            model_id, default_context, default_effort, effort_options_json, context_options_json, openai_endpoint, openai_extra_params_enabled,
            openai_extra_params_json, custom_headers_enabled, custom_headers_json,
            anthropic_extra_params_enabled, anthropic_extra_params_json, context_window_tokens,
            max_completion_tokens, anthropic_max_tokens,
            thinking_budget_tokens, created_at_ms, updated_at_ms
        ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"#,
    )
        .bind(hash)
        .bind(input.sort_order)
        .bind(&input.display_name)
        .bind(&input.group_name)
        .bind(input.model_type.as_str())
        .bind(&input.base_url)
        .bind(input.use_full_url)
        .bind(&input.api_key)
        .bind(&input.tooltip_data)
        .bind(&input.model_id)
        .bind(&input.default_context)
        .bind(&input.default_effort)
        .bind(serde_json::to_string(&input.effort_options)?)
        .bind(serde_json::to_string(&input.context_options)?)
        .bind(&input.openai_endpoint)
        .bind(input.openai_extra_params_enabled)
        .bind(serde_json::to_string(&input.openai_extra_params)?)
        .bind(input.custom_headers_enabled)
        .bind(serde_json::to_string(&input.custom_headers)?)
        .bind(input.anthropic_extra_params_enabled)
        .bind(serde_json::to_string(&input.anthropic_extra_params)?)
        .bind(input.context_window_tokens.map(to_i64).transpose()?)
        .bind(input.max_completion_tokens.map(to_i64).transpose()?)
        .bind(input.anthropic_max_tokens.map(to_i64).transpose()?)
        .bind(input.thinking_budget_tokens.map(to_i64).transpose()?)
        .bind(now)
        .bind(now)
        .execute(&mut **transaction)
        .await?;
    Ok(())
}

fn model_from_row(row: sqlx::sqlite::SqliteRow) -> Result<ModelConfig> {
    Ok(ModelConfig {
        model_hash: row.try_get("model_hash")?,
        sort_order: row.try_get("sort_order")?,
        display_name: row.try_get("display_name")?,
        group_name: row.try_get("group_name")?,
        model_type: ModelType::from_str(row.try_get("model_type")?)?,
        base_url: row.try_get("base_url")?,
        use_full_url: row.try_get("use_full_url")?,
        api_key: row.try_get("api_key")?,
        tooltip_data: row.try_get("tooltip_data")?,
        model_id: row.try_get("model_id")?,
        default_context: row.try_get("default_context")?,
        default_effort: row.try_get("default_effort")?,
        effort_options: serde_json::from_str(
            row.try_get::<String, _>("effort_options_json")?.as_str(),
        )?,
        context_options: serde_json::from_str(
            row.try_get::<String, _>("context_options_json")?.as_str(),
        )?,
        openai_endpoint: row.try_get("openai_endpoint")?,
        openai_extra_params_enabled: row.try_get("openai_extra_params_enabled")?,
        openai_extra_params: serde_json::from_str(
            row.try_get::<String, _>("openai_extra_params_json")?
                .as_str(),
        )?,
        custom_headers_enabled: row.try_get("custom_headers_enabled")?,
        custom_headers: serde_json::from_str(
            row.try_get::<String, _>("custom_headers_json")?.as_str(),
        )?,
        anthropic_extra_params_enabled: row.try_get("anthropic_extra_params_enabled")?,
        anthropic_extra_params: serde_json::from_str(
            row.try_get::<String, _>("anthropic_extra_params_json")?
                .as_str(),
        )?,
        context_window_tokens: optional_u64(&row, "context_window_tokens")?,
        max_completion_tokens: optional_u64(&row, "max_completion_tokens")?,
        anthropic_max_tokens: optional_u64(&row, "anthropic_max_tokens")?,
        thinking_budget_tokens: optional_u64(&row, "thinking_budget_tokens")?,
        created_at_ms: row.try_get("created_at_ms")?,
        updated_at_ms: row.try_get("updated_at_ms")?,
    })
}

fn optional_u64(row: &sqlx::sqlite::SqliteRow, column: &str) -> Result<Option<u64>> {
    row.try_get::<Option<i64>, _>(column)?
        .map(|value| {
            u64::try_from(value).map_err(|_| Error::Config(format!("{column} cannot be negative")))
        })
        .transpose()
}

fn to_i64(value: u64) -> Result<i64> {
    i64::try_from(value).map_err(|_| Error::Config("token value is too large".into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model_input(group_name: Option<&str>) -> ModelConfigInput {
        ModelConfigInput {
            sort_order: 0,
            display_name: "Test Model".into(),
            group_name: group_name.map(String::from),
            model_type: ModelType::OpenAi,
            base_url: "https://example.com/v1/chat/completions".into(),
            use_full_url: true,
            api_key: "test-key".into(),
            tooltip_data: "Test Model".into(),
            model_id: "test-model".into(),
            default_context: None,
            default_effort: None,
            effort_options: Vec::new(),
            context_options: Vec::new(),
            openai_endpoint: crate::model::OPENAI_CHAT_ENDPOINT.into(),
            openai_extra_params_enabled: false,
            openai_extra_params: serde_json::json!({}),
            custom_headers_enabled: false,
            custom_headers: serde_json::json!({}),
            anthropic_extra_params_enabled: false,
            anthropic_extra_params: serde_json::json!({}),
            context_window_tokens: None,
            max_completion_tokens: None,
            anthropic_max_tokens: None,
            thinking_budget_tokens: None,
        }
    }

    /// 分组名是纯展示字段:入库时去除首尾空白、空串归一为 NULL,
    /// 更新分组名不得改变模型身份哈希。
    #[tokio::test]
    async fn group_name_round_trips_without_changing_model_identity() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::connect(&format!(
            "sqlite://{}",
            directory.path().join("test.db").display()
        ))
        .await
        .unwrap();

        let created = store
            .create_model(&model_input(Some("  My Group  ")))
            .await
            .unwrap();
        assert_eq!(created.group_name.as_deref(), Some("My Group"));

        let renamed = store
            .update_model(&created.model_hash, &model_input(Some("Renamed")))
            .await
            .unwrap();
        assert_eq!(renamed.model_hash, created.model_hash);
        assert_eq!(renamed.group_name.as_deref(), Some("Renamed"));

        let cleared = store
            .update_model(&created.model_hash, &model_input(Some("   ")))
            .await
            .unwrap();
        assert_eq!(cleared.model_hash, created.model_hash);
        assert_eq!(cleared.group_name, None);
    }
    fn input(name: &str) -> ModelConfigInput {
        ModelConfigInput {
            sort_order: 1,
            display_name: name.into(),
            group_name: None,
            model_type: ModelType::OpenAi,
            base_url: "https://example.com/v1/responses".into(),
            use_full_url: true,
            api_key: "secret".into(),
            tooltip_data: "Example model".into(),
            model_id: "model-a".into(),
            default_context: Some("1m".into()),
            default_effort: Some("high".into()),
            effort_options: vec!["low".into(), "high".into()],
            context_options: vec!["200k".into(), "1m".into()],
            openai_endpoint: "/v1/responses".into(),
            openai_extra_params_enabled: true,
            openai_extra_params: serde_json::json!({"service_tier":"priority"}),
            custom_headers_enabled: true,
            custom_headers: serde_json::json!({"x-client":"cursor-byok"}),
            anthropic_extra_params_enabled: false,
            anthropic_extra_params: serde_json::json!({}),
            context_window_tokens: Some(200_000),
            max_completion_tokens: Some(8_192),
            anthropic_max_tokens: None,
            thinking_budget_tokens: None,
        }
    }

    #[tokio::test]
    async fn model_configuration_round_trips_and_updates_identity() {
        let store = Store::connect("sqlite::memory:").await.unwrap();
        let created = store.create_model(&input("Model A")).await.unwrap();
        assert_eq!(created.model_hash.len(), 8);
        assert_eq!(created.custom_headers["x-client"], "cursor-byok");
        assert_eq!(store.models().await.unwrap().len(), 1);

        // 展示名不再参与身份:改名保持 ID。
        let renamed = store
            .update_model(&created.model_hash, &input("Renamed"))
            .await
            .unwrap();
        assert_eq!(renamed.model_hash, created.model_hash);

        // 上游模型名或密钥变化才改变身份。
        let mut rekeyed = input("Model A");
        rekeyed.api_key = "another-key".into();
        let updated = store
            .update_model(&created.model_hash, &rekeyed)
            .await
            .unwrap();
        assert_ne!(updated.model_hash, created.model_hash);
        assert!(store.model(&created.model_hash).await.unwrap().is_none());

        store.delete_model(&updated.model_hash).await.unwrap();
        assert!(store.models().await.unwrap().is_empty());
    }

    /// 显式默认档位随新列入库、读取与更新往返;清除显式值时按参数轴首项兜底(DDL 为 NOT NULL)。
    #[tokio::test]
    async fn default_variant_columns_round_trip() {
        let store = Store::connect("sqlite::memory:").await.unwrap();
        let created = store.create_model(&input("Model A")).await.unwrap();
        assert_eq!(created.default_context.as_deref(), Some("1m"));
        assert_eq!(created.default_effort.as_deref(), Some("high"));

        let mut updated_input = input("Model A");
        updated_input.default_context = Some(" 200K ".into());
        updated_input.default_effort = Some("low".into());
        let updated = store
            .update_model(&created.model_hash, &updated_input)
            .await
            .unwrap();
        assert_eq!(updated.default_context.as_deref(), Some("200k"));
        assert_eq!(updated.default_effort.as_deref(), Some("low"));

        let cleared_input = {
            let mut cleared = input("Model A");
            cleared.default_context = None;
            cleared.default_effort = None;
            cleared
        };
        let cleared = store
            .update_model(&created.model_hash, &cleared_input)
            .await
            .unwrap();
        assert_eq!(cleared.default_context.as_deref(), Some("200k"));
        assert_eq!(cleared.default_effort.as_deref(), Some("low"));
    }

    /// M-1:保存入口是 Effort 取值归一的唯一防线。自定义轴(首项无效)、
    /// 全无效轴与空轴分别过滤、回填默认五档;默认档位无效取轴首项,
    /// 不再走旧白名单的 400。
    #[tokio::test]
    async fn effort_axes_are_sanitized_only_at_save_entry_points() {
        let store = Store::connect("sqlite::memory:").await.unwrap();

        // 旧锁死复现:首项自定义 + 显式该值默认 → 保存成功并归一。
        let mut custom = input("Custom Axis");
        custom.effort_options = vec!["turbo".into(), "low".into(), "HIGH ".into()];
        custom.default_effort = Some("turbo".into());
        let saved = store.create_model(&custom).await.unwrap();
        assert_eq!(saved.effort_options, vec!["low", "high"]);
        assert_eq!(saved.default_effort.as_deref(), Some("low"));

        // null 默认 + 首项自定义(裸 API 隐式落库形态)同样归一。
        let mut implicit = input("Implicit Default");
        implicit.model_id = "model-implicit".into();
        implicit.effort_options = vec!["turbo".into(), "high".into()];
        implicit.default_effort = None;
        let saved = store.create_model(&implicit).await.unwrap();
        assert_eq!(saved.effort_options, vec!["high"]);
        assert_eq!(saved.default_effort.as_deref(), Some("high"));

        // 全无效轴:回填默认五档;轴外默认被替换为首项,轴内默认保留。
        let mut invalid = input("All Invalid");
        invalid.model_id = "model-invalid".into();
        invalid.effort_options = vec!["turbo".into(), "  ".into()];
        invalid.default_effort = Some("turbo".into());
        let saved = store.create_model(&invalid).await.unwrap();
        assert_eq!(
            saved.effort_options,
            crate::model::DEFAULT_EFFORT_OPTIONS
                .iter()
                .map(|value| String::from(*value))
                .collect::<Vec<String>>()
        );
        assert_eq!(saved.default_effort.as_deref(), Some("low"));
        invalid.model_id = "model-invalid-kept".into();
        invalid.default_effort = Some("high".into());
        let saved = store.create_model(&invalid).await.unwrap();
        assert_eq!(saved.default_effort.as_deref(), Some("high"));

        // none/minimal/xhigh/max 在轴上原样保留,默认取有效值。
        let mut extended = input("Extended Axis");
        extended.model_id = "model-extended".into();
        extended.effort_options = vec![
            " NONE ".into(),
            "Minimal".into(),
            "xhigh".into(),
            "max".into(),
        ];
        extended.default_effort = Some("MAX".into());
        let saved = store.create_model(&extended).await.unwrap();
        assert_eq!(
            saved.effort_options,
            vec!["none", "minimal", "xhigh", "max"]
        );
        assert_eq!(saved.default_effort.as_deref(), Some("max"));
    }

    /// M-1:读取不清洗(历史非法配置原样可读、可作目录轴),下一次保存
    /// 通过入口后自愈。
    #[tokio::test]
    async fn persisted_invalid_effort_reads_verbatim_and_self_heals_on_next_save() {
        let store = Store::connect("sqlite::memory:").await.unwrap();
        let created = store.create_model(&input("Model A")).await.unwrap();
        // 模拟历史隐式落库:直接 UPDATE 到白名单外的轴与默认。
        sqlx::query(
            "UPDATE model_configs SET effort_options_json = ?, default_effort = ? WHERE model_hash = ?",
        )
        .bind(r#"["turbo","low"]"#)
        .bind("turbo")
        .bind(&created.model_hash)
        .execute(store.pool())
        .await
        .unwrap();

        // 读取路径不做任何清洗。
        let stored = store.model(&created.model_hash).await.unwrap().unwrap();
        assert_eq!(stored.effort_options, vec!["turbo", "low"]);
        assert_eq!(stored.default_effort.as_deref(), Some("turbo"));

        // 下一次保存(编辑/复制/分组保存同走 normalize_model_input)自愈。
        let saved = store
            .update_model(&created.model_hash, &stored.into_input())
            .await
            .unwrap();
        assert_eq!(saved.effort_options, vec!["low"]);
        assert_eq!(saved.default_effort.as_deref(), Some("low"));
        let stored = store.model(&created.model_hash).await.unwrap().unwrap();
        assert_eq!(stored.effort_options, vec!["low"]);
        assert_eq!(stored.default_effort.as_deref(), Some("low"));
    }

    /// M-1:额外参数是高级透传,不受 Effort 白名单/子集校验约束,原样入库。
    #[tokio::test]
    async fn extra_params_remain_unvalidated_passthrough() {
        let store = Store::connect("sqlite::memory:").await.unwrap();
        let mut input = input("Extra Params");
        input.openai_extra_params = serde_json::json!({
            "reasoning_effort": "turbo",
            "effort": "ultra",
            "service_tier": "priority"
        });
        let saved = store.create_model(&input).await.unwrap();
        assert_eq!(
            saved.openai_extra_params["reasoning_effort"], "turbo",
            "额外参数不经 Effort 白名单"
        );
        assert_eq!(saved.openai_extra_params["effort"], "ultra");
        assert_eq!(saved.openai_extra_params["service_tier"], "priority");
    }

    #[tokio::test]
    async fn model_lookup_only_matches_the_exact_unified_id() {
        let store = Store::connect("sqlite::memory:").await.unwrap();
        let saved = store.create_model(&input("Model A")).await.unwrap();

        // 精确 ID 命中;名称、大小写变体与变体 slug 都不再由 store 匹配。
        assert_eq!(
            store
                .model(&saved.model_hash)
                .await
                .unwrap()
                .map(|model| model.model_hash),
            Some(saved.model_hash.clone())
        );
        for key in [
            "model-a".to_string(),
            "Model A".to_string(),
            "MODEL-A".to_string(),
            format!("{}-1m-low", saved.model_hash),
        ] {
            assert!(
                store.model(&key).await.unwrap().is_none(),
                "key {key} must not resolve in the store layer"
            );
        }
    }

    #[tokio::test]
    async fn commit_names_and_slugs_are_saved_as_base_ids() {
        let store = Store::connect("sqlite::memory:").await.unwrap();
        let model = store.create_model(&input("Named Model")).await.unwrap();
        for key in [
            "Named Model".to_owned(),
            format!("{}-1m-low-fast", model.model_hash),
        ] {
            let saved = store
                .set_commit_settings(crate::store::CommitSettings {
                    model_id: key.clone(),
                    ..Default::default()
                })
                .await
                .unwrap();
            assert_eq!(saved.model_id, model.model_hash);
            if key.ends_with("-fast") {
                assert_eq!(saved.parameters.context.as_deref(), Some("1m"));
                assert_eq!(saved.parameters.reasoning.as_deref(), Some("low"));
                assert_eq!(saved.parameters.fast, Some(true));
            }
        }
        assert!(store
            .set_commit_settings(crate::store::CommitSettings {
                model_id: "unknown".into(),
                ..Default::default()
            })
            .await
            .is_err());
    }

    #[tokio::test]
    async fn checked_writes_reject_cross_source_ids_before_persistence() {
        let store = Store::connect("sqlite::memory:").await.unwrap();
        let value = input("Builtin");
        let descriptor = crate::plugin::PluginModelDescriptor {
            id: model_hash(&value).unwrap(),
            plugin_id: "test".into(),
            plugin_name: "Test".into(),
            provider_id: "provider".into(),
            model_id: "upstream".into(),
            display_name: "Plugin".into(),
            description: None,
            icon: String::new(),
            provider_type: "openai".into(),
            max_output_tokens: None,
            images: false,
            enabled: true,
            effort_options: vec![],
            context_options: vec![],
            default_effort: None,
            default_context: None,
        };
        assert!(store
            .create_models_checked(
                std::slice::from_ref(&value),
                std::slice::from_ref(&descriptor)
            )
            .await
            .is_err());
        assert!(store.models().await.unwrap().is_empty());
        let mut other = value.clone();
        other.model_id = "different".into();
        let existing = store.create_model(&other).await.unwrap();
        assert!(store
            .update_model_checked(&existing.model_hash, &value, &[descriptor])
            .await
            .is_err());
        assert!(store.model(&existing.model_hash).await.unwrap().is_some());
        assert!(store.create_model(&other).await.is_err());
    }

    #[tokio::test]
    async fn batch_creation_is_atomic() {
        let store = Store::connect("sqlite::memory:").await.unwrap();
        let duplicate = input("Model A");
        assert!(store
            .create_models(&[duplicate.clone(), duplicate])
            .await
            .is_err());
        assert!(store.models().await.unwrap().is_empty());
    }

    /// 分组设置整批更新:改名改址生效、占位符密钥回填、显式新密钥换身份、
    /// 未编辑字段保持不变。
    #[tokio::test]
    async fn group_batch_merges_edits_and_backfills_placeholder_keys() {
        let store = Store::connect("sqlite::memory:").await.unwrap();
        let mut first_input = input("First");
        first_input.model_id = "model-first".into();
        first_input.base_url = "https://first.example.com/v1/chat/completions".into();
        let first = store.create_model(&first_input).await.unwrap();
        let mut second_input = input("Second");
        second_input.model_id = "model-second".into();
        second_input.group_name = Some("Old".into());
        let second = store.create_model(&second_input).await.unwrap();

        let saved = store
            .update_models_group(
                &[
                    crate::model::ModelGroupEdit {
                        model_hash: first.model_hash.clone(),
                        group_name: Some(" Group ".into()),
                        base_url: None,
                        api_key: Some(crate::model::REDACTED_SECRET.into()),
                    },
                    crate::model::ModelGroupEdit {
                        model_hash: second.model_hash.clone(),
                        group_name: Some("Group".into()),
                        base_url: Some("https://group.example.com/v1/chat/completions".into()),
                        api_key: Some("new-key".into()),
                    },
                ],
                &[],
            )
            .await
            .unwrap();

        // 占位符密钥与未编辑字段回填自现有配置:身份不变。
        let first_after = &saved
            .iter()
            .find(|model| model.model_hash == first.model_hash)
            .expect("placeholder key back-fill keeps model identity");
        assert_eq!(first_after.api_key, "secret");
        assert_eq!(first_after.group_name.as_deref(), Some("Group"));
        assert_eq!(first_after.base_url, first.base_url);
        assert_eq!(first_after.model_id, "model-first");
        assert_eq!(first_after.effort_options, first.effort_options);
        assert_eq!(first_after.default_effort, first.default_effort);
        assert_eq!(first_after.context_options, first.context_options);
        assert_eq!(first_after.openai_extra_params, first.openai_extra_params);

        // 显式新密钥与新地址换身份:旧哈希消失,新哈希可读(身份 = URL+密钥+上游名)。
        let second_after = saved
            .iter()
            .find(|model| model.model_hash != first.model_hash)
            .expect("edited key re-identifies the model");
        assert_ne!(second_after.model_hash, second.model_hash);
        assert!(store.model(&second.model_hash).await.unwrap().is_none());
        assert_eq!(second_after.api_key, "new-key");
        assert_eq!(second_after.group_name.as_deref(), Some("Group"));
        assert_eq!(
            second_after.base_url,
            "https://group.example.com/v1/chat/completions"
        );
        assert_eq!(second_after.model_id, "model-second");
    }

    #[tokio::test]
    async fn group_patch_keeps_unedited_name_and_can_clear_it() {
        let store = Store::connect("sqlite::memory:").await.unwrap();
        let mut original = input("Group patch");
        original.group_name = Some("Beta".into());
        let model = store.create_model(&original).await.unwrap();
        let mut edit = crate::model::ModelGroupEdit {
            model_hash: model.model_hash.clone(),
            group_name: None,
            base_url: None,
            api_key: Some(crate::model::REDACTED_SECRET.into()),
        };
        store.update_models_group(&[edit], &[]).await.unwrap();
        assert_eq!(
            store
                .model(&model.model_hash)
                .await
                .unwrap()
                .unwrap()
                .group_name
                .as_deref(),
            Some("Beta")
        );
        edit = crate::model::ModelGroupEdit {
            model_hash: model.model_hash.clone(),
            group_name: Some(String::new()),
            base_url: None,
            api_key: None,
        };
        store.update_models_group(&[edit], &[]).await.unwrap();
        assert_eq!(
            store
                .model(&model.model_hash)
                .await
                .unwrap()
                .unwrap()
                .group_name,
            None
        );
    }

    /// 坏条目(缺失模型 / 校验失败)整批回滚:任何模型都不落库。
    #[tokio::test]
    async fn group_batch_fails_atomically_without_partial_application() {
        let store = Store::connect("sqlite::memory:").await.unwrap();
        let mut first_input = input("First");
        first_input.model_id = "model-first".into();
        let first = store.create_model(&first_input).await.unwrap();
        let mut second_input = input("Second");
        second_input.model_id = "model-second".into();
        let second = store.create_model(&second_input).await.unwrap();

        // 丢失模型:目标哈希不存在,整批失败。
        let missing = store
            .update_models_group(
                &[
                    crate::model::ModelGroupEdit {
                        model_hash: first.model_hash.clone(),
                        group_name: Some("Group".into()),
                        base_url: None,
                        api_key: None,
                    },
                    crate::model::ModelGroupEdit {
                        model_hash: "missing".into(),
                        group_name: Some("Group".into()),
                        base_url: None,
                        api_key: None,
                    },
                ],
                &[],
            )
            .await;
        assert!(matches!(missing, Err(Error::RunNotFound(_))));
        assert_eq!(
            store
                .model(&first.model_hash)
                .await
                .unwrap()
                .unwrap()
                .group_name,
            None
        );

        // 坏条目:无效 URL 过不了归一校验,整批失败。
        let invalid = store
            .update_models_group(
                &[
                    crate::model::ModelGroupEdit {
                        model_hash: first.model_hash.clone(),
                        group_name: Some("Group".into()),
                        base_url: None,
                        api_key: None,
                    },
                    crate::model::ModelGroupEdit {
                        model_hash: second.model_hash.clone(),
                        group_name: None,
                        base_url: Some("not-a-url".into()),
                        api_key: None,
                    },
                ],
                &[],
            )
            .await;
        assert!(invalid.is_err());
        assert_eq!(
            store
                .model(&first.model_hash)
                .await
                .unwrap()
                .unwrap()
                .group_name,
            None
        );
        assert_eq!(
            store
                .model(&second.model_hash)
                .await
                .unwrap()
                .unwrap()
                .group_name,
            None
        );
        assert_eq!(store.models().await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn model_order_is_replaced_atomically() {
        let store = Store::connect("sqlite::memory:").await.unwrap();
        let first = store.create_model(&input("First")).await.unwrap();
        let mut second_input = input("Second");
        second_input.model_id = "model-b".into();
        second_input.sort_order = 2;
        let second = store.create_model(&second_input).await.unwrap();

        let reordered = store
            .reorder_models(&[second.model_hash.clone(), first.model_hash.clone()])
            .await
            .unwrap();
        assert_eq!(reordered[0].model_hash, second.model_hash);
        assert_eq!(reordered[0].sort_order, 1);
        assert_eq!(reordered[1].model_hash, first.model_hash);
        assert_eq!(reordered[1].sort_order, 2);

        assert!(store
            .reorder_models(std::slice::from_ref(&first.model_hash))
            .await
            .is_err());
        assert_eq!(
            store.models().await.unwrap()[0].model_hash,
            second.model_hash
        );
    }
}
