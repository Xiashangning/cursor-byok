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
    use crate::model::{ModelType, OPENAI_CHAT_ENDPOINT};

    #[test]
    fn model_responses_omit_provider_credentials() {
        let config = ModelConfig {
            model_hash: "hash".into(),
            sort_order: 0,
            display_name: "Model".into(),
            group_name: None,
            model_type: ModelType::OpenAi,
            base_url: "https://example.com".into(),
            use_full_url: false,
            api_key: "secret".into(),
            tooltip_data: String::new(),
            model_id: "model".into(),
            reasoning_effort: None,
            effort_options: Vec::new(),
            context_options: Vec::new(),
            openai_endpoint: OPENAI_CHAT_ENDPOINT.into(),
            openai_extra_params_enabled: false,
            openai_extra_params: serde_json::json!({}),
            custom_headers_enabled: true,
            custom_headers: serde_json::json!({
                "Authorization": "Bearer secret",
                "X-Api-Key": "secret",
                "X-Tenant": "public"
            }),
            anthropic_extra_params_enabled: false,
            anthropic_extra_params: serde_json::json!({}),
            context_window_tokens: None,
            max_completion_tokens: None,
            anthropic_max_tokens: None,
            anthropic_thinking_effort: None,
            thinking_budget_tokens: None,
            created_at_ms: 0,
            updated_at_ms: 0,
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
}
