//! Publishes the desktop management routes to a local agent under /byok/app/v1.
//!
//! 鉴权复用控制 API 的访问令牌中间件（见 `api_router` 中的 `auth::require`）：
//! 回环来源放行，非回环来源必须携带 `Authorization: Bearer <访问令牌>`。
//! 本模块只负责开关检查与路径重写转发，不引入独立的密钥体系。
use axum::{
    body::Body,
    extract::State,
    http::{Request, StatusCode, Uri},
    response::{IntoResponse, Response},
    routing::any,
    Json, Router,
};
use serde_json::json;
use tower::ServiceExt;

use super::ControlService;

#[derive(Clone)]
struct Gate {
    service: ControlService,
    api: Router,
}

pub fn attach(service: ControlService, api: Router) -> Router {
    let gate = Gate {
        service,
        api: api.clone(),
    };
    api.merge(
        Router::new()
            .route("/byok/app/v1", any(forward))
            .route("/byok/app/v1/{*path}", any(forward))
            .with_state(gate),
    )
}

async fn forward(State(gate): State<Gate>, request: Request<Body>) -> Response {
    let settings = match gate.service.app_api_settings().await {
        Ok(settings) => settings,
        Err(error) => return api_error(StatusCode::INTERNAL_SERVER_ERROR, error),
    };
    if !settings.enabled {
        return api_error(StatusCode::FORBIDDEN, "application API is disabled");
    }
    let (mut parts, body) = request.into_parts();
    let Ok(uri) = rewrite(&parts.uri) else {
        return api_error(StatusCode::BAD_REQUEST, "invalid application API path");
    };
    parts.uri = uri;
    match gate
        .api
        .clone()
        .oneshot(Request::from_parts(parts, body))
        .await
    {
        Ok(response) => response,
        Err(error) => match error {},
    }
}

fn rewrite(uri: &Uri) -> Result<Uri, axum::http::uri::InvalidUri> {
    let rest = uri
        .path()
        .trim_start_matches("/byok/app/v1")
        .trim_start_matches('/');
    let mut path = if rest.is_empty() {
        "/__byok-api__/api".to_owned()
    } else {
        format!("/__byok-api__/api/{rest}")
    };
    if let Some(query) = uri.query() {
        path.push('?');
        path.push_str(query);
    }
    path.parse()
}

fn api_error(status: StatusCode, message: impl std::fmt::Display) -> Response {
    let code = if status == StatusCode::FORBIDDEN {
        "permission_denied"
    } else if status.is_client_error() {
        "invalid_argument"
    } else {
        "internal"
    };
    (
        status,
        Json(json!({"code": code, "message": message.to_string()})),
    )
        .into_response()
}
