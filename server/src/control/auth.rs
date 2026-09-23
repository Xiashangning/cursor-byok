//! Guard for the control API: loopback clients pass, remote clients need a Bearer token.
use std::{
    net::SocketAddr,
    sync::{Arc, RwLock},
};

use axum::{
    body::Body,
    extract::{ConnectInfo, FromRequestParts, State},
    http::{header, request::Parts, Request, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
    Json,
};
use serde::Serialize;

use crate::{store::Store, Error, Result};

#[derive(Clone)]
pub struct AccessToken {
    current: Arc<RwLock<String>>,
    regeneration: Arc<tokio::sync::Mutex<()>>,
    source: AccessTokenSource,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AccessTokenSource {
    /// CURSOR_ACCESS_TOKEN 固定;服务端不可重新生成。
    Environment,
    /// 自动生成并持久化;可在设置页重新生成。
    Generated,
}

impl AccessToken {
    /// 环境变量优先;否则读库,没有则生成并持久化。
    pub async fn resolve(store: &Store, environment: Option<String>) -> Result<Self> {
        if let Some(token) = environment
            .map(|token| token.trim().to_owned())
            .filter(|token| !token.is_empty())
        {
            return Ok(Self::new(token, AccessTokenSource::Environment));
        }
        let token = match store.access_token().await? {
            Some(token) if !token.is_empty() => token,
            _ => {
                let token = generate();
                store.set_access_token(&token).await?;
                token
            }
        };
        Ok(Self::new(token, AccessTokenSource::Generated))
    }

    pub(crate) fn new(token: String, source: AccessTokenSource) -> Self {
        Self {
            current: Arc::new(RwLock::new(token)),
            regeneration: Arc::new(tokio::sync::Mutex::new(())),
            source,
        }
    }

    pub fn current(&self) -> String {
        self.current
            .read()
            .expect("access token lock poisoned")
            .clone()
    }

    pub fn source(&self) -> AccessTokenSource {
        self.source
    }

    /// 生成并切换到新令牌;环境变量固定的令牌不可重新生成。
    pub async fn regenerate(&self, store: &Store) -> Result<String> {
        if self.source == AccessTokenSource::Environment {
            return Err(Error::Config(
                "access token is set by CURSOR_ACCESS_TOKEN; unset it to regenerate".into(),
            ));
        }
        let _regeneration = self.regeneration.lock().await;
        let token = generate();
        store.set_access_token(&token).await?;
        *self.current.write().expect("access token lock poisoned") = token.clone();
        Ok(token)
    }
}

fn generate() -> String {
    format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    )
}

/// 对端地址;进程内调用(测试)没有 ConnectInfo,视为回环放行。
pub(crate) struct PeerAddr(Option<SocketAddr>);

impl<S> FromRequestParts<S> for PeerAddr
where
    S: Send + Sync,
{
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(
        parts: &mut Parts,
        _state: &S,
    ) -> std::result::Result<Self, Self::Rejection> {
        Ok(Self(
            parts
                .extensions
                .get::<ConnectInfo<SocketAddr>>()
                .map(|peer| peer.0),
        ))
    }
}

/// 回环来源(桌面 WebView、本机 CLI、测试)直接放行;
/// 非回环来源必须携带 Authorization: Bearer <access token>。
pub(crate) async fn require(
    State(token): State<AccessToken>,
    peer: PeerAddr,
    request: Request<Body>,
    next: Next,
) -> Response {
    let loopback = peer.0.is_none_or(|address| address.ip().is_loopback());
    let authorized = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .is_some_and(|presented| presented == token.current());
    if loopback || authorized {
        return next.run(request).await;
    }
    (
        StatusCode::UNAUTHORIZED,
        [(header::WWW_AUTHENTICATE, "Bearer")],
        Json(serde_json::json!({
            "code": "unauthenticated",
            "message": "remote access requires Authorization: Bearer <access token>"
        })),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{routing::get, Router};
    use tower::ServiceExt;

    fn app(token: &str) -> Router {
        Router::new()
            .route("/protected", get(|| async { StatusCode::NO_CONTENT }))
            .layer(axum::middleware::from_fn_with_state(
                AccessToken::new(token.into(), AccessTokenSource::Generated),
                require,
            ))
    }

    fn request(peer: Option<&str>, authorization: Option<&str>) -> Request<Body> {
        let mut builder = Request::builder().uri("/protected");
        if let Some(authorization) = authorization {
            builder = builder.header(header::AUTHORIZATION, authorization);
        }
        let mut request = builder.body(Body::empty()).unwrap();
        if let Some(peer) = peer {
            request
                .extensions_mut()
                .insert(ConnectInfo(peer.parse::<SocketAddr>().unwrap()));
        }
        request
    }

    #[tokio::test]
    async fn loopback_peers_pass_without_a_token() {
        for peer in [None, Some("127.0.0.1:5000"), Some("[::1]:5000")] {
            let response = app("secret").oneshot(request(peer, None)).await.unwrap();
            assert_eq!(response.status(), StatusCode::NO_CONTENT, "peer: {peer:?}");
        }
    }

    #[tokio::test]
    async fn remote_peers_need_the_bearer_token() {
        let unauthorized = app("secret")
            .oneshot(request(Some("192.168.1.10:5000"), None))
            .await
            .unwrap();
        assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);

        let wrong = app("secret")
            .oneshot(request(Some("192.168.1.10:5000"), Some("Bearer wrong")))
            .await
            .unwrap();
        assert_eq!(wrong.status(), StatusCode::UNAUTHORIZED);

        let authorized = app("secret")
            .oneshot(request(Some("192.168.1.10:5000"), Some("Bearer secret")))
            .await
            .unwrap();
        assert_eq!(authorized.status(), StatusCode::NO_CONTENT);
    }

    #[tokio::test]
    async fn regeneration_rotates_the_accepted_token() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::connect(&format!(
            "sqlite://{}",
            directory.path().join("token.db").display()
        ))
        .await
        .unwrap();
        let token = AccessToken::resolve(&store, None).await.unwrap();
        assert_eq!(token.source(), AccessTokenSource::Generated);
        assert_eq!(token.current().len(), 64);
        // 重启后从库里读到同一个令牌。
        let reloaded = AccessToken::resolve(&store, None).await.unwrap();
        assert_eq!(reloaded.current(), token.current());

        let rotated = token.regenerate(&store).await.unwrap();
        assert_ne!(rotated, reloaded.current());
        assert_eq!(
            store.access_token().await.unwrap().as_deref(),
            Some(rotated.as_str())
        );
    }

    #[tokio::test]
    async fn concurrent_regeneration_keeps_memory_and_storage_in_sync() {
        const CONCURRENCY: usize = 16;

        let directory = tempfile::tempdir().unwrap();
        let store = Store::connect(&format!(
            "sqlite://{}",
            directory.path().join("concurrent-token.db").display()
        ))
        .await
        .unwrap();
        let token = AccessToken::resolve(&store, None).await.unwrap();
        let barrier = Arc::new(tokio::sync::Barrier::new(CONCURRENCY));
        let mut tasks = tokio::task::JoinSet::new();

        for _ in 0..CONCURRENCY {
            let token = token.clone();
            let store = store.clone();
            let barrier = barrier.clone();
            tasks.spawn(async move {
                barrier.wait().await;
                token.regenerate(&store).await.unwrap()
            });
        }

        let mut generated = Vec::with_capacity(CONCURRENCY);
        while let Some(result) = tasks.join_next().await {
            generated.push(result.unwrap());
        }

        let current = token.current();
        assert!(generated.contains(&current));
        assert_eq!(
            store.access_token().await.unwrap().as_deref(),
            Some(current.as_str())
        );
    }

    #[tokio::test]
    async fn environment_token_wins_and_cannot_be_regenerated() {
        let store = Store::connect("sqlite::memory:").await.unwrap();
        let token = AccessToken::resolve(&store, Some("  fixed  ".into()))
            .await
            .unwrap();
        assert_eq!(token.source(), AccessTokenSource::Environment);
        assert_eq!(token.current(), "fixed");
        assert!(token.regenerate(&store).await.is_err());
        // 环境变量不写库。
        assert_eq!(store.access_token().await.unwrap(), None);
    }
}
