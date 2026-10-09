//! Implements model configuration endpoints.
use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};
use serde::Deserialize;

use crate::{
    model::{ModelConfig, ModelConfigInput},
    Result,
};

use super::{ControlService, DiscoveredModels, ModelConnectivityResult, ModelDiscoveryInput};

#[derive(Deserialize)]
pub struct SaveModels {
    pub models: Vec<ModelConfigInput>,
}

#[derive(Deserialize)]
pub struct ModelOrder {
    pub model_hashes: Vec<String>,
}

/// 分组设置整批请求:每条只携带用户编辑的字段,服务端读取现有配置合并。
#[derive(Deserialize)]
pub struct GroupModelsRequest {
    pub edits: Vec<crate::model::ModelGroupEdit>,
}

#[derive(Deserialize)]
pub struct DuplicateModel {
    pub display_name: String,
    #[serde(default)]
    pub sort_order: i64,
}

pub async fn list(State(service): State<ControlService>) -> Result<Json<Vec<ModelConfig>>> {
    Ok(Json(redact(service.models().await?)))
}

pub async fn create(
    State(service): State<ControlService>,
    Json(input): Json<SaveModels>,
) -> Result<(StatusCode, Json<Vec<ModelConfig>>)> {
    Ok((
        StatusCode::CREATED,
        Json(redact(service.create_models(&input.models).await?)),
    ))
}

pub async fn reorder(
    State(service): State<ControlService>,
    Json(input): Json<ModelOrder>,
) -> Result<Json<Vec<ModelConfig>>> {
    Ok(Json(redact(
        service.reorder_models(&input.model_hashes).await?,
    )))
}

pub async fn remove(
    State(service): State<ControlService>,
    Path(model_hash): Path<String>,
) -> Result<StatusCode> {
    service.delete_model(&model_hash).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// 复制模型:密钥与自定义头从存储中的现有配置克隆,不经过脱敏往返。
pub async fn duplicate(
    State(service): State<ControlService>,
    Path(model_hash): Path<String>,
    Json(input): Json<DuplicateModel>,
) -> Result<(StatusCode, Json<ModelConfig>)> {
    let model = service
        .duplicate_model(&model_hash, input.display_name, input.sort_order)
        .await?
        .redact_secrets();
    Ok((StatusCode::CREATED, Json(model)))
}

pub async fn update(
    State(service): State<ControlService>,
    Path(model_hash): Path<String>,
    Json(input): Json<ModelConfigInput>,
) -> Result<Json<ModelConfig>> {
    Ok(Json(
        service
            .update_model(&model_hash, &input)
            .await?
            .redact_secrets(),
    ))
}

/// 分组设置整批保存:一次请求合并并原子应用全部条目,失败整体回滚。
pub async fn update_group(
    State(service): State<ControlService>,
    Json(input): Json<GroupModelsRequest>,
) -> Result<Json<Vec<ModelConfig>>> {
    Ok(Json(redact(
        service.update_model_group(&input.edits).await?,
    )))
}

pub async fn test(
    State(service): State<ControlService>,
    Path((model_hash, test_id)): Path<(String, String)>,
) -> Result<Json<ModelConnectivityResult>> {
    Ok(Json(service.test_model(&model_hash, &test_id).await?))
}

pub async fn cancel(
    State(service): State<ControlService>,
    Path((_model_hash, test_id)): Path<(String, String)>,
) -> Result<StatusCode> {
    service.cancel_model_test(&test_id);
    Ok(StatusCode::NO_CONTENT)
}

pub async fn discover(
    State(service): State<ControlService>,
    Json(input): Json<ModelDiscoveryInput>,
) -> Result<Json<DiscoveredModels>> {
    Ok(Json(service.discover_models(&input).await?))
}

fn redact(models: Vec<ModelConfig>) -> Vec<ModelConfig> {
    models
        .into_iter()
        .map(ModelConfig::redact_secrets)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::OPENAI_CHAT_ENDPOINT;

    #[test]
    fn model_responses_omit_provider_credentials() {
        let config = ModelConfig {
            model_hash: "hash".into(),
            display_name: "Model".into(),
            base_url: "https://example.com".into(),
            api_key: "secret".into(),
            model_id: "model".into(),
            openai_endpoint: OPENAI_CHAT_ENDPOINT.into(),
            custom_headers_enabled: true,
            custom_headers: serde_json::json!({
                "Authorization": "Bearer secret",
                "X-Api-Key": "secret",
                "X-Tenant": "public"
            }),
            ..Default::default()
        };

        let value = serde_json::to_value(config.redact_secrets()).unwrap();
        assert_eq!(value["api_key"], crate::model::REDACTED_SECRET);
        assert_eq!(
            value["custom_headers"],
            serde_json::json!({
                "Authorization": crate::model::REDACTED_SECRET,
                "X-Api-Key": crate::model::REDACTED_SECRET,
                "X-Tenant": "public"
            })
        );
        assert!(!value.to_string().contains("secret"));
    }

    /// V-9:分组设置整批保存——占位符密钥回填、未编辑字段保持、坏条目整体回滚。
    #[tokio::test]
    async fn control_group_batch_applies_atomically_with_placeholder_backfill() {
        let store = crate::store::Store::connect("sqlite::memory:")
            .await
            .unwrap();
        let service = super::super::test_support::service_for(store.clone());
        let mut first_input = crate::model::ModelConfigInput {
            sort_order: 0,
            display_name: "First".into(),
            group_name: None,
            model_type: crate::model::ModelType::OpenAi,
            base_url: "https://first.example.com/v1/responses".into(),
            use_full_url: true,
            api_key: "secret".into(),
            tooltip_data: "First".into(),
            model_id: "model-first".into(),
            default_context: None,
            default_effort: None,
            effort_options: Vec::new(),
            context_options: Vec::new(),
            openai_endpoint: crate::model::OPENAI_RESPONSES_ENDPOINT.into(),
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
        };
        first_input.effort_options = vec!["low".into(), "high".into()];
        let first = store.create_model(&first_input).await.unwrap();
        let mut second_input = first_input.clone();
        second_input.model_id = "model-second".into();
        let second = store.create_model(&second_input).await.unwrap();

        // 成功批:占位符密钥回填(身份不变),编辑字段生效,响应脱敏。
        let saved = service
            .update_model_group(&[
                crate::model::ModelGroupEdit {
                    model_hash: first.model_hash.clone(),
                    group_name: Some("Group".into()),
                    base_url: None,
                    api_key: Some(crate::model::REDACTED_SECRET.into()),
                },
                crate::model::ModelGroupEdit {
                    model_hash: second.model_hash.clone(),
                    group_name: Some("Group".into()),
                    base_url: None,
                    api_key: Some(crate::model::REDACTED_SECRET.into()),
                },
            ])
            .await
            .unwrap();
        assert_eq!(saved.len(), 2);
        let first_after = saved
            .iter()
            .find(|model| model.model_hash == first.model_hash)
            .expect("placeholder key keeps identity");
        // 服务层返回未脱敏配置(响应脱敏由 handler 的 redact 统一负责);
        // 占位符回填保持密钥原值与身份。
        assert_eq!(first_after.api_key, "secret");
        assert_eq!(first_after.group_name.as_deref(), Some("Group"));
        assert_eq!(first_after.model_hash, first.model_hash);
        // 坏条目(缺失模型):整批失败,已应用条目保持原状。
        let failed = service
            .update_model_group(&[
                crate::model::ModelGroupEdit {
                    model_hash: first.model_hash.clone(),
                    group_name: Some("Renamed".into()),
                    base_url: None,
                    api_key: None,
                },
                crate::model::ModelGroupEdit {
                    model_hash: "missing".into(),
                    group_name: Some("Renamed".into()),
                    base_url: None,
                    api_key: None,
                },
            ])
            .await;
        assert!(failed.is_err());
        let after_failure = store.model(&first.model_hash).await.unwrap().unwrap();
        assert_eq!(after_failure.group_name.as_deref(), Some("Group"));
    }

    /// M-1:控制层创建/更新/复制入口与存储层共用 model::configuration 的
    /// Effort 归一 —— 桌面分组保存与编辑器保存(POST /models、
    /// PUT /models/{hash})不再因自定义轴首项或非法默认档位 400,
    /// 历史锁死配置在下次保存时自愈。
    #[tokio::test]
    async fn control_save_entries_normalize_effort_axes_without_rejecting() {
        let store = crate::store::Store::connect("sqlite::memory:")
            .await
            .unwrap();
        let service = super::super::test_support::service_for(store);
        let mut input = crate::model::ModelConfigInput {
            sort_order: 0,
            display_name: "Custom Axis".into(),
            group_name: None,
            model_type: crate::model::ModelType::OpenAi,
            base_url: "https://example.com/v1/responses".into(),
            use_full_url: true,
            api_key: "secret".into(),
            tooltip_data: "Custom Axis".into(),
            model_id: "model-a".into(),
            default_context: None,
            default_effort: Some("turbo".into()),
            effort_options: vec!["turbo".into(), "low".into(), "high".into()],
            context_options: Vec::new(),
            openai_endpoint: crate::model::OPENAI_RESPONSES_ENDPOINT.into(),
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
        };

        // 分组保存/编辑器保存共用的批量创建入口:保存成功且轴已归一。
        let created = service.create_models(&[input.clone()]).await.unwrap();
        assert_eq!(created[0].effort_options, vec!["low", "high"]);
        assert_eq!(created[0].default_effort.as_deref(), Some("low"));

        // 更新入口同样归一(此前该形态保存必 400,模型被锁死)。
        input.effort_options = vec!["ultra".into()];
        input.default_effort = Some("ultra".into());
        let updated = service
            .update_model(&created[0].model_hash, &input)
            .await
            .unwrap();
        assert_eq!(
            updated.effort_options,
            crate::model::DEFAULT_EFFORT_OPTIONS
                .iter()
                .map(|value| String::from(*value))
                .collect::<Vec<String>>()
        );
        assert_eq!(updated.default_effort.as_deref(), Some("low"));

        // 复制入口走同一归一:锁死链路的最后一环。
        let copy = service
            .duplicate_model(&updated.model_hash, "Copy".into(), 2)
            .await
            .unwrap();
        assert_eq!(copy.effort_options, updated.effort_options);
        assert_eq!(copy.default_effort, updated.default_effort);
    }
}
