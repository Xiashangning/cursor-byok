//! Exposes plugin discovery, resource lifecycle, model sync, and runtime endpoints.
use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};
use serde::Deserialize;

use crate::{
    plugin::{
        ImportResponse, OAuthBeginResponse, OAuthPollResponse, PluginDescriptor,
        PluginRuntimeStatus,
    },
    Result,
};

use super::ControlService;

pub async fn list(State(service): State<ControlService>) -> Result<Json<Vec<PluginDescriptor>>> {
    Ok(Json(service.plugins().await))
}

pub async fn remove(
    State(service): State<ControlService>,
    Path(plugin_id): Path<String>,
) -> Result<StatusCode> {
    service.remove_plugin_configuration(&plugin_id).await?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn oauth_begin(
    State(service): State<ControlService>,
    Path((plugin_id, resource_type, method_id)): Path<(String, String, String)>,
) -> Result<Json<OAuthBeginResponse>> {
    Ok(Json(
        service
            .plugin_oauth_begin(&plugin_id, &resource_type, &method_id)
            .await?,
    ))
}

pub async fn oauth_poll(
    State(service): State<ControlService>,
    Path(session_id): Path<String>,
) -> Result<Json<OAuthPollResponse>> {
    Ok(Json(service.plugin_oauth_poll(&session_id).await?))
}

pub async fn import(
    State(service): State<ControlService>,
    Path((plugin_id, resource_type)): Path<(String, String)>,
    Json(files): Json<serde_json::Value>,
) -> Result<Json<ImportResponse>> {
    Ok(Json(
        service
            .plugin_import(&plugin_id, &resource_type, files)
            .await?,
    ))
}

/// 以附件形式返回账号资源导出文件,便于浏览器直接下载。
pub async fn export_resources(
    State(service): State<ControlService>,
    Path((plugin_id, resource_type)): Path<(String, String)>,
) -> Result<axum::response::Response> {
    let value = service
        .plugin_export_resources(&plugin_id, &resource_type)
        .await?;
    let body = serde_json::to_vec_pretty(&value)?;
    let response = axum::response::Response::builder()
        .header(axum::http::header::CONTENT_TYPE, "application/json")
        .header(
            axum::http::header::CONTENT_DISPOSITION,
            format!("attachment; filename=\"{plugin_id}-{resource_type}.json\""),
        )
        .body(axum::body::Body::from(body))
        .expect("static export response");
    Ok(response)
}

pub async fn refresh_resource(
    State(service): State<ControlService>,
    Path((plugin_id, resource_type, resource_id)): Path<(String, String, String)>,
) -> Result<StatusCode> {
    service
        .plugin_refresh_resource(&plugin_id, &resource_type, &resource_id)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn delete_resource(
    State(service): State<ControlService>,
    Path((plugin_id, resource_type, resource_id)): Path<(String, String, String)>,
) -> Result<StatusCode> {
    service
        .plugin_delete_resource(&plugin_id, &resource_type, &resource_id)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn sync_models(
    State(service): State<ControlService>,
    Path((plugin_id, provider_id)): Path<(String, String)>,
) -> Result<Json<serde_json::Value>> {
    let count = service.plugin_sync_models(&plugin_id, &provider_id).await?;
    Ok(Json(serde_json::json!({ "models": count })))
}

pub async fn runtime_status(
    State(service): State<ControlService>,
) -> Result<Json<PluginRuntimeStatus>> {
    Ok(Json(service.plugin_runtime_status()))
}

pub async fn initialize_runtime(
    State(service): State<ControlService>,
) -> Result<Json<PluginRuntimeStatus>> {
    Ok(Json(service.initialize_plugin_runtime()))
}

pub async fn cancel_runtime_initialization(
    State(service): State<ControlService>,
) -> Result<Json<PluginRuntimeStatus>> {
    Ok(Json(service.cancel_plugin_runtime_initialization()))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetDisabledModelsInput {
    pub model_ids: Vec<String>,
}

pub async fn get_disabled_models(
    State(service): State<ControlService>,
) -> Result<Json<Vec<String>>> {
    Ok(Json(service.disabled_plugin_models().await?))
}

pub async fn set_disabled_models(
    State(service): State<ControlService>,
    Json(input): Json<SetDisabledModelsInput>,
) -> Result<Json<Vec<String>>> {
    service.set_disabled_plugin_models(input.model_ids).await?;
    Ok(Json(service.disabled_plugin_models().await?))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetDisabledAccountsInput {
    pub account_ids: Vec<String>,
}

pub async fn get_disabled_accounts(
    State(service): State<ControlService>,
) -> Result<Json<Vec<String>>> {
    Ok(Json(service.disabled_plugin_accounts().await?))
}

pub async fn set_disabled_accounts(
    State(service): State<ControlService>,
    Json(input): Json<SetDisabledAccountsInput>,
) -> Result<Json<Vec<String>>> {
    service
        .set_disabled_plugin_accounts(input.account_ids)
        .await?;
    Ok(Json(service.disabled_plugin_accounts().await?))
}
