//! Lightweight local OAuth callback server for plugin OAuth redirect flows.
use std::{collections::HashMap, net::SocketAddr, sync::Arc};
use axum::{
    extract::{Query, State},
    response::Html,
    routing::get,
    Json, Router,
};
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};

#[derive(Default, Clone)]
pub struct OAuthCallbackState {
    codes: Arc<RwLock<HashMap<String, String>>>,
}

#[derive(Deserialize)]
pub struct CallbackQuery {
    pub code: Option<String>,
    pub state: Option<String>,
    pub error: Option<String>,
}

#[derive(Deserialize)]
pub struct StatusQuery {
    pub state: Option<String>,
}

#[derive(Serialize)]
pub struct StatusResponse {
    pub code: Option<String>,
}

pub async fn start_oauth_callback_server(shutdown: tokio_util::sync::CancellationToken) {
    let port = 51121;
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let state = OAuthCallbackState::default();

    let router = Router::new()
        .route("/oauth-callback", get(handle_callback))
        .route("/auth-status", get(handle_status))
        .with_state(state);

    let listener = match tokio::net::TcpListener::bind(addr).await {
        Ok(l) => l,
        Err(err) => {
            tracing::warn!(%addr, %err, "OAuth callback port 51121 unavailable or already bound");
            return;
        }
    };
    tracing::info!(%addr, "OAuth callback server listening");

    let server = axum::serve(listener, router).with_graceful_shutdown(async move {
        shutdown.cancelled().await;
    });

    if let Err(err) = server.await {
        tracing::debug!(%err, "OAuth callback server stopped");
    }
}

async fn handle_callback(
    State(state): State<OAuthCallbackState>,
    Query(query): Query<CallbackQuery>,
) -> Html<&'static str> {
    if let (Some(code), Some(st)) = (query.code, query.state) {
        state.codes.write().insert(st, code);
    }
    Html(r#"<!DOCTYPE html>
<html>
<head>
    <meta charset="utf-8">
    <title>Authorization Successful</title>
    <style>
        body { font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, sans-serif; background: #18181b; color: #f4f4f5; display: flex; align-items: center; justify-content: center; height: 100vh; margin: 0; }
        .card { background: #27272a; border: 1px solid #3f3f46; padding: 36px; border-radius: 16px; text-align: center; box-shadow: 0 10px 30px rgba(0,0,0,0.5); max-width: 440px; }
        .icon { font-size: 40px; color: #4285f4; margin-bottom: 16px; }
        h1 { color: #ffffff; font-size: 20px; margin: 0 0 12px 0; }
        p { color: #a1a1aa; font-size: 14px; line-height: 1.6; margin: 0; }
    </style>
</head>
<body>
    <div class="card">
        <div class="icon">✓</div>
        <h1>Authorization Successful</h1>
        <p>Your account has been authorized. You can close this browser tab and return to <strong>Cursor BYOK</strong>.</p>
    </div>
</body>
</html>"#)
}

async fn handle_status(
    State(state): State<OAuthCallbackState>,
    Query(query): Query<StatusQuery>,
) -> Json<StatusResponse> {
    let code = query.state.and_then(|st| state.codes.read().get(&st).cloned());
    Json(StatusResponse { code })
}
