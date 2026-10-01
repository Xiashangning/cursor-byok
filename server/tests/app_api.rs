//! Verifies the local-agent management API (/byok/app/v1) gate end to end.
mod support;

use std::{sync::Arc, time::Duration};

use support::{openai_model_input, temp_store};

use axum::{
    body::{to_bytes, Body},
    http::{header, Request, StatusCode},
    Router,
};
use cursor_server::{
    control::{self, AccessToken},
    network::NetworkClients,
    plugin::{PluginRegistry, PluginRuntime},
    provider::ProviderRouter,
    store::AppApiSettings,
};
use serde_json::{json, Value};
use tower::ServiceExt;

const ACCESS_TOKEN: &str = "test-access-token";

async fn setup() -> (Router, cursor_server::store::Store) {
    let (_directory, store) = temp_store().await;
    let runtime = PluginRuntime::managed().unwrap();
    let plugins = PluginRegistry::managed(store.clone(), runtime.clone(), "0.1.0".into()).unwrap();
    let provider = Arc::new(ProviderRouter::new(
        store.clone(),
        plugins.clone(),
        NetworkClients::new(store.clone()),
        Duration::from_secs(5),
        Duration::from_secs(5),
    ));
    let access_token = AccessToken::resolve(&store, Some(ACCESS_TOKEN.into()))
        .await
        .unwrap();
    let control = control::ControlService::new(
        store.clone(),
        provider,
        runtime,
        plugins,
        NetworkClients::new(store.clone()),
        access_token,
    )
    .unwrap();
    (control::api_router(control), store)
}

async fn send(
    router: Router,
    method: &str,
    path: &str,
    authorization: Option<&str>,
    peer: Option<&str>,
    body: Value,
) -> (StatusCode, String) {
    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .header(header::CONTENT_TYPE, "application/json");
    if let Some(authorization) = authorization {
        request = request.header(header::AUTHORIZATION, authorization);
    }
    let mut request = request.body(Body::from(body.to_string())).unwrap();
    if let Some(peer) = peer {
        request.extensions_mut().insert(axum::extract::ConnectInfo(
            peer.parse::<std::net::SocketAddr>().unwrap(),
        ));
    }
    let response = router.oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    (status, String::from_utf8(bytes.to_vec()).unwrap())
}

#[tokio::test]
async fn disabled_application_api_rejects_agents_without_blocking_the_desktop() {
    let (router, _store) = setup().await;
    let (status, body) = send(
        router.clone(),
        "GET",
        "/byok/app/v1/models",
        None,
        None,
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(
        serde_json::from_str::<Value>(&body).unwrap()["message"],
        "application API is disabled"
    );
    assert_eq!(
        send(
            router,
            "GET",
            "/__byok-api__/api/models",
            None,
            None,
            json!({}),
        )
        .await
        .0,
        StatusCode::OK
    );
}

#[tokio::test]
async fn enabled_application_api_can_add_a_model_and_change_settings() {
    let (router, store) = setup().await;
    store
        .set_app_api_settings(AppApiSettings { enabled: true })
        .await
        .unwrap();
    let (status, _) = send(
        router.clone(),
        "POST",
        "/byok/app/v1/models",
        None,
        None,
        json!({ "models": [openai_model_input("agent-model", None)] }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, body) = send(
        router.clone(),
        "GET",
        "/byok/app/v1/models",
        None,
        None,
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let models = serde_json::from_str::<Value>(&body).unwrap();
    assert_eq!(models[0]["model_id"], "agent-model");

    let (status, body) = send(
        router,
        "PUT",
        "/byok/app/v1/settings/observability",
        None,
        None,
        json!({ "detailed": false }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        serde_json::from_str::<Value>(&body).unwrap()["detailed"],
        false
    );
}

#[tokio::test]
async fn application_api_reuses_the_control_access_token_for_remote_peers() {
    let (router, store) = setup().await;
    store
        .set_app_api_settings(AppApiSettings { enabled: true })
        .await
        .unwrap();
    let remote = Some("192.168.1.10:5000");

    // 非回环来源没有令牌:拒绝。
    assert_eq!(
        send(
            router.clone(),
            "GET",
            "/byok/app/v1/models",
            None,
            remote,
            json!({})
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
    // 非回环来源携带错误的令牌:拒绝。
    assert_eq!(
        send(
            router.clone(),
            "GET",
            "/byok/app/v1/models",
            Some("Bearer wrong"),
            remote,
            json!({}),
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
    // 非回环来源携带访问令牌:放行。
    assert_eq!(
        send(
            router.clone(),
            "GET",
            "/byok/app/v1/models",
            Some(&format!("Bearer {ACCESS_TOKEN}")),
            remote,
            json!({}),
        )
        .await
        .0,
        StatusCode::OK
    );
    // 内部管理路由同样受访问令牌约束。
    assert_eq!(
        send(
            router,
            "GET",
            "/__byok-api__/api/models",
            None,
            remote,
            json!({}),
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
}
