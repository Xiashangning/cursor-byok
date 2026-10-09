//! Model discovery for the control API: probes provider endpoints for their
//! model catalogs and normalizes the returned result.
use reqwest::header::{HeaderName, HeaderValue};
use serde::{Deserialize, Serialize};
use url::Url;

use crate::{
    model::{ModelType, ProviderType},
    Error, Result,
};

use super::service::{empty_json_object_ref, restore_redacted_headers, ControlService};

#[derive(Clone, Debug, Serialize)]
pub struct DiscoveredModels {
    pub models: Vec<String>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct ModelDiscoveryInput {
    #[serde(rename = "type")]
    pub model_type: ModelType,
    pub base_url: String,
    pub api_key: String,
    /// 编辑已有模型时带上哈希:编辑器往返的是脱敏配置(REDACTED_SECRET 占位符),
    /// 占位符密钥/敏感头由服务端回填。
    #[serde(default)]
    pub model_hash: Option<String>,
    #[serde(default)]
    pub custom_headers_enabled: bool,
    #[serde(default = "empty_json_object")]
    pub custom_headers: serde_json::Value,
}

fn empty_json_object() -> serde_json::Value {
    serde_json::json!({})
}

impl ControlService {
    pub async fn discover_models(&self, input: &ModelDiscoveryInput) -> Result<DiscoveredModels> {
        let (api_key, custom_headers) = self.discovery_credentials(input).await?;
        let client = self.clients.default_client().await?;
        let base_url = crate::model::normalize_request_url(&input.base_url)?;
        discover_models_from_endpoint(
            &client,
            match input.model_type {
                ModelType::OpenAi => ProviderType::OpenAiResponses,
                ModelType::Anthropic => ProviderType::Anthropic,
            },
            &base_url,
            &api_key,
            if input.custom_headers_enabled {
                &custom_headers
            } else {
                empty_json_object_ref()
            },
        )
        .await
    }

    /// 编辑已有模型时,编辑器里往返的是脱敏配置(REDACTED_SECRET 占位符),
    /// 发现模型前从存储回填;占位符语义与更新模型一致(空串即「清除」,不回填)。
    /// 一旦回填了存储的密钥,目标 URL 必须仍是该模型自己配置的地址,
    /// 否则等于把密钥发给调用方指定的任意服务器。
    async fn discovery_credentials(
        &self,
        input: &ModelDiscoveryInput,
    ) -> Result<(String, serde_json::Value)> {
        let Some(model_hash) = input.model_hash.as_deref() else {
            return Ok((input.api_key.clone(), input.custom_headers.clone()));
        };
        let stored = self
            .store
            .model(model_hash)
            .await?
            .ok_or_else(|| Error::RunNotFound(format!("model {model_hash}")))?;
        let redacted_key = input.api_key == crate::model::REDACTED_SECRET;
        let api_key = if redacted_key {
            stored.api_key.clone()
        } else {
            input.api_key.clone()
        };
        let mut custom_headers = input.custom_headers.clone();
        let backfilled_headers =
            restore_redacted_headers(&mut custom_headers, &stored.custom_headers);
        if (redacted_key && !api_key.is_empty()) || backfilled_headers > 0 {
            let requested = crate::model::normalize_request_url(&input.base_url)?;
            let configured = crate::model::normalize_request_url(&stored.base_url)?;
            if requested.trim_end_matches('/') != configured.trim_end_matches('/') {
                return Err(Error::Config(
                    "model discovery with stored credentials requires the model's configured base URL; re-enter the API key to discover from a different URL".into(),
                ));
            }
        }
        Ok((api_key, custom_headers))
    }
}

async fn discover_models_from_endpoint(
    client: &reqwest::Client,
    provider_type: ProviderType,
    base_url: &str,
    api_key: &str,
    custom_headers: &serde_json::Value,
) -> Result<DiscoveredModels> {
    let mut models = match provider_type {
        ProviderType::OpenAiChat | ProviderType::OpenAiResponses => {
            openai_models(client, base_url, api_key, custom_headers).await?
        }
        ProviderType::Anthropic => {
            anthropic_models(client, base_url, api_key, custom_headers).await?
        }
        ProviderType::Plugin => {
            return Err(Error::Config(
                "plugin providers discover models through their plugin".into(),
            ))
        }
    };
    models.sort();
    models.dedup();
    Ok(DiscoveredModels { models })
}

fn model_discovery_url(base_url: &str) -> Result<Url> {
    let mut url = Url::parse(base_url)
        .map_err(|error| Error::Config(format!("invalid model request URL: {error}")))?;
    if url.host_str().is_none() {
        return Err(Error::Config(
            "model request URL must contain a host".into(),
        ));
    }
    // 在现有路径上追加，而不是整段替换：多数编程套餐的 API 挂在子路径下
    // （/api/anthropic、/coding、/api/paas/v4 等），直接 set_path("/v1/models")
    // 会把这些前缀吃掉，发现请求必然 404
    let path = url.path().trim_end_matches('/');
    let versioned = crate::model::has_trailing_version(path);
    let new_path = if let Some(parent) = path.strip_suffix("/chat/completions") {
        // 完整请求 URL：剥掉端点段（chat/completions 是两段），换成 models
        format!("{parent}/models")
    } else if let Some(parent) = path
        .strip_suffix("/responses")
        .or_else(|| path.strip_suffix("/messages"))
        .or_else(|| path.strip_suffix("/completions"))
    {
        format!("{parent}/models")
    } else if path.is_empty() {
        "/v1/models".to_string()
    } else if versioned {
        // 已带版本段（/v1、/api/v3、/api/paas/v4）：只补 models
        format!("{path}/models")
    } else {
        format!("{path}/v1/models")
    };
    url.set_path(&new_path);
    url.set_query(None);
    url.set_fragment(None);
    Ok(url)
}

fn model_discovery_urls(base_url: &str) -> Result<Vec<Url>> {
    let mut configured = Url::parse(base_url)
        .map_err(|error| Error::Config(format!("invalid model request URL: {error}")))?;
    let path = configured.path().trim_end_matches('/');
    let tail = path.rsplit('/').next().unwrap_or_default();
    if matches!(tail.to_ascii_lowercase().as_str(), "model" | "models") {
        configured.set_query(None);
        configured.set_fragment(None);
        return Ok(vec![configured]);
    }

    let primary = model_discovery_url(base_url)?;
    let versioned = crate::model::has_trailing_version(path);
    let complete_request_url = [
        "/chat/completions",
        "/responses",
        "/messages",
        "/completions",
    ]
    .iter()
    .any(|suffix| path.to_ascii_lowercase().ends_with(suffix));
    if versioned || complete_request_url {
        return Ok(vec![primary]);
    }

    let Some(prefix) = primary.path().strip_suffix("/v1/models") else {
        return Ok(vec![primary]);
    };
    let mut fallback = primary.clone();
    fallback.set_path(&format!("{prefix}/models"));
    Ok(vec![primary, fallback])
}

async fn openai_models(
    client: &reqwest::Client,
    base_url: &str,
    api_key: &str,
    custom_headers: &serde_json::Value,
) -> Result<Vec<String>> {
    let mut last_error = None;
    for url in model_discovery_urls(base_url)? {
        match openai_models_at(client, url, api_key, custom_headers).await {
            Ok(models) => return Ok(models),
            Err(error) => last_error = Some(error),
        }
    }
    Err(last_error.unwrap_or_else(|| Error::Provider("no model discovery URL available".into())))
}

async fn openai_models_at(
    client: &reqwest::Client,
    url: Url,
    api_key: &str,
    custom_headers: &serde_json::Value,
) -> Result<Vec<String>> {
    let mut request = client.get(url);
    if !api_key.is_empty() {
        request = request.bearer_auth(api_key);
    }
    let response = apply_discovery_headers(request, custom_headers)?
        .send()
        .await?;
    let status = response.status();
    let body: serde_json::Value = response.json().await?;
    if !status.is_success() {
        return Err(Error::Provider(format!(
            "model discovery failed ({status}): {body}"
        )));
    }
    Ok(model_ids(body.get("data").unwrap_or(&body)))
}

async fn anthropic_models(
    client: &reqwest::Client,
    base_url: &str,
    api_key: &str,
    custom_headers: &serde_json::Value,
) -> Result<Vec<String>> {
    let mut last_error = None;
    for url in model_discovery_urls(base_url)? {
        match anthropic_models_at(client, url, api_key, custom_headers).await {
            Ok(models) => return Ok(models),
            Err(error) => last_error = Some(error),
        }
    }
    Err(last_error.unwrap_or_else(|| Error::Provider("no model discovery URL available".into())))
}

async fn anthropic_models_at(
    client: &reqwest::Client,
    url: Url,
    api_key: &str,
    custom_headers: &serde_json::Value,
) -> Result<Vec<String>> {
    let mut after_id = None::<String>;
    let mut found = std::collections::BTreeSet::new();
    loop {
        let mut request = client
            .get(url.clone())
            .query(&[("limit", "100")])
            .header("anthropic-version", "2023-06-01");
        if !api_key.is_empty() {
            request = request.header("x-api-key", api_key);
        }
        if let Some(after_id) = &after_id {
            request = request.query(&[("after_id", after_id)]);
        }
        let response = apply_discovery_headers(request, custom_headers)?
            .send()
            .await?;
        let status = response.status();
        let body: serde_json::Value = response.json().await?;
        if !status.is_success() {
            return Err(Error::Provider(format!(
                "model discovery failed ({status}): {body}"
            )));
        }
        found.extend(model_ids(body.get("data").unwrap_or(&body)));
        if body.get("has_more").and_then(serde_json::Value::as_bool) != Some(true) {
            break;
        }
        after_id = body
            .get("last_id")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned);
        if after_id.is_none() {
            return Err(Error::Provider(
                "Anthropic model response has_more without last_id".into(),
            ));
        }
    }
    Ok(found.into_iter().collect())
}

fn model_ids(value: &serde_json::Value) -> Vec<String> {
    value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|item| match item {
            serde_json::Value::String(id) => Some(id.clone()),
            serde_json::Value::Object(object) => object
                .get("id")
                .or_else(|| object.get("name"))
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned),
            _ => None,
        })
        .collect()
}

fn apply_discovery_headers(
    mut request: reqwest::RequestBuilder,
    headers: &serde_json::Value,
) -> Result<reqwest::RequestBuilder> {
    let object = headers
        .as_object()
        .ok_or_else(|| Error::Config("custom headers must be an object".into()))?;
    for (name, value) in object {
        if name.eq_ignore_ascii_case("user-agent") {
            continue;
        }
        let value = value
            .as_str()
            .ok_or_else(|| Error::Config(format!("custom header {name} must be a string")))?;
        let name = HeaderName::try_from(name)
            .map_err(|error| Error::Config(format!("invalid header name: {error}")))?;
        let value = HeaderValue::try_from(value)
            .map_err(|error| Error::Config(format!("invalid header value: {error}")))?;
        request = request.header(name, value);
    }
    Ok(request)
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use crate::model::ModelType;
    use crate::plugin::{PluginRegistry, PluginRuntime};
    use crate::store::Store;

    use super::super::test_support::{service_for, TestProvider};
    use super::super::{AccessToken, AccessTokenSource, ControlService};
    use super::{model_discovery_url, model_discovery_urls};

    #[test]
    fn model_discovery_url_appends_to_path() {
        let cases = [
            (
                "https://api.deepseek.com",
                "https://api.deepseek.com/v1/models",
            ),
            (
                "https://open.bigmodel.cn/api/anthropic",
                "https://open.bigmodel.cn/api/anthropic/v1/models",
            ),
            (
                "https://api.kimi.com/coding",
                "https://api.kimi.com/coding/v1/models",
            ),
            (
                "https://api.moonshot.cn/v1",
                "https://api.moonshot.cn/v1/models",
            ),
            (
                "https://ark.cn-beijing.volces.com/api/v3",
                "https://ark.cn-beijing.volces.com/api/v3/models",
            ),
            (
                "https://open.bigmodel.cn/api/coding/paas/v4/chat/completions",
                "https://open.bigmodel.cn/api/coding/paas/v4/models",
            ),
            (
                "https://example.com:8443/arbitrary/v1/chat/completions",
                "https://example.com:8443/arbitrary/v1/models",
            ),
        ];
        for (base, expected) in cases {
            assert_eq!(
                model_discovery_url(base).unwrap().as_str(),
                expected,
                "base: {base}"
            );
        }
    }

    #[test]
    fn model_discovery_urls_fall_back_without_a_version() {
        let cases = [
            (
                "https://opencode.ai/zen/go/v1",
                vec!["https://opencode.ai/zen/go/v1/models"],
            ),
            (
                "https://opencode.ai/zen/go",
                vec![
                    "https://opencode.ai/zen/go/v1/models",
                    "https://opencode.ai/zen/go/models",
                ],
            ),
            (
                "https://api.example.com/openai/v1/models",
                vec!["https://api.example.com/openai/v1/models"],
            ),
        ];
        for (base, expected) in cases {
            let actual = model_discovery_urls(base)
                .unwrap()
                .into_iter()
                .map(|url| url.to_string())
                .collect::<Vec<_>>();
            assert_eq!(actual, expected, "base: {base}");
        }
    }

    #[tokio::test]
    async fn openai_model_discovery_uses_the_unversioned_fallback() {
        let app = axum::Router::new().route(
            "/proxy/models",
            axum::routing::get(|| async {
                axum::Json(serde_json::json!({ "data": [{ "id": "model-a" }] }))
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let models = super::openai_models(
            &reqwest::Client::new(),
            &format!("http://{address}/proxy"),
            "secret",
            &serde_json::json!({}),
        )
        .await
        .unwrap();

        assert_eq!(models, vec!["model-a"]);
        server.abort();
    }

    #[tokio::test]
    async fn model_discovery_does_not_inherit_user_agent_or_request_body_settings() {
        type CapturedRequest = (
            axum::http::Method,
            axum::http::Uri,
            axum::http::HeaderMap,
            bytes::Bytes,
        );

        async fn models(
            axum::extract::State(sender): axum::extract::State<
                tokio::sync::mpsc::UnboundedSender<CapturedRequest>,
            >,
            request: axum::extract::Request,
        ) -> axum::Json<serde_json::Value> {
            let (parts, body) = request.into_parts();
            let body = axum::body::to_bytes(body, usize::MAX).await.unwrap();
            sender
                .send((parts.method, parts.uri, parts.headers, body))
                .unwrap();
            axum::Json(serde_json::json!({ "data": [{ "id": "model-a" }] }))
        }

        let (sender, mut requests) = tokio::sync::mpsc::unbounded_channel();
        let app = axum::Router::new()
            .route("/custom/models", axum::routing::get(models))
            .with_state(sender);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let directory = tempfile::tempdir().unwrap();
        let store = Store::connect(&format!(
            "sqlite://{}",
            directory.path().join("discovery.db").display()
        ))
        .await
        .unwrap();
        let plugin_runtime = PluginRuntime::managed().unwrap();
        let plugins =
            PluginRegistry::managed(store.clone(), plugin_runtime.clone(), "test".into()).unwrap();
        let clients = crate::network::NetworkClients::new(store.clone());
        let service = ControlService::new(
            store,
            Arc::new(TestProvider {
                invocation: Arc::new(Mutex::new(None)),
            }),
            plugin_runtime,
            plugins,
            clients,
            AccessToken::new("test".into(), AccessTokenSource::Generated),
        )
        .unwrap();
        let result = service
            .discover_models(&super::ModelDiscoveryInput {
                model_type: ModelType::OpenAi,
                base_url: format!("http://{address}/custom/responses"),
                api_key: "secret".into(),
                model_hash: None,
                custom_headers_enabled: true,
                custom_headers: serde_json::json!({
                    "uSeR-aGeNt": "inherited-user-agent",
                    "x-tenant": "tenant-a"
                }),
            })
            .await
            .unwrap();

        assert_eq!(result.models, vec!["model-a"]);
        let (method, uri, headers, body) = requests.recv().await.unwrap();
        assert_eq!(method, axum::http::Method::GET);
        // /custom/responses 剥掉端点段后是 /custom，发现地址为 /custom/models
        assert_eq!(uri.path(), "/custom/models");
        assert!(body.is_empty());
        assert!(headers.get(axum::http::header::USER_AGENT).is_none());
        assert_eq!(headers.get("x-tenant").unwrap(), "tenant-a");
        assert_eq!(
            headers.get(axum::http::header::AUTHORIZATION).unwrap(),
            "Bearer secret"
        );
        server.abort();
    }

    /// 回填存储密钥时,目标 URL 必须仍是模型自己配置的地址,
    /// 否则存储的密钥会被发往调用方指定的任意服务器。
    #[tokio::test]
    async fn discovery_with_stored_credentials_requires_the_configured_base_url() {
        type CapturedRequest = axum::http::HeaderMap;

        async fn models(
            axum::extract::State(sender): axum::extract::State<
                tokio::sync::mpsc::UnboundedSender<CapturedRequest>,
            >,
            request: axum::extract::Request,
        ) -> axum::Json<serde_json::Value> {
            sender.send(request.headers().clone()).unwrap();
            axum::Json(serde_json::json!({ "data": [{ "id": "model-a" }] }))
        }

        let (sender, mut requests) = tokio::sync::mpsc::unbounded_channel();
        let app = axum::Router::new()
            .route("/custom/models", axum::routing::get(models))
            .with_state(sender);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let directory = tempfile::tempdir().unwrap();
        let store = Store::connect(&format!(
            "sqlite://{}",
            directory.path().join("discovery-gate.db").display()
        ))
        .await
        .unwrap();
        let service = service_for(store.clone());
        let mut input = super::super::test_support::model_with_headers(&store)
            .await
            .into_input();
        input.base_url = format!("http://{address}/custom/responses");
        let created = store.create_model(&input).await.unwrap();

        let discover = |base_url: String, api_key: String| super::ModelDiscoveryInput {
            model_type: ModelType::OpenAi,
            base_url,
            api_key,
            model_hash: Some(created.model_hash.clone()),
            custom_headers_enabled: false,
            custom_headers: serde_json::json!({}),
        };

        // 改了 base_url、密钥保留占位符(等回填)→ 拒绝。
        let rejected = service
            .discover_models(&discover(
                "https://attacker.example.com/v1/responses".into(),
                crate::model::REDACTED_SECRET.into(),
            ))
            .await;
        assert!(matches!(rejected, Err(crate::Error::Config(_))));

        // 空白与尾随斜杠差异不算改地址,回填的存储密钥正常发出。
        let allowed = service
            .discover_models(&discover(
                format!(" http://{address}/custom/responses/ "),
                crate::model::REDACTED_SECRET.into(),
            ))
            .await
            .unwrap();
        assert_eq!(allowed.models, vec!["model-a"]);
        let headers = requests.recv().await.unwrap();
        assert_eq!(
            headers.get(axum::http::header::AUTHORIZATION).unwrap(),
            "Bearer secret"
        );

        // 显式提供新密钥时不涉及存储密钥,允许任意地址。
        let allowed = service
            .discover_models(&discover(format!("http://{address}"), "fresh-key".into()))
            .await;
        // 未回填则不设限;该地址的 /v1/models 不存在,失败是 404 而非门禁拒绝。
        assert!(
            !matches!(allowed, Err(crate::Error::Config(_))),
            "{allowed:?}"
        );
        server.abort();
    }

    /// 编辑器往返:脱敏后的占位符在发现模型时由服务端从存储回填。
    #[tokio::test]
    async fn discovery_falls_back_to_stored_credentials_when_redacted() {
        type CapturedRequest = (axum::http::Uri, axum::http::HeaderMap);

        async fn models(
            axum::extract::State(sender): axum::extract::State<
                tokio::sync::mpsc::UnboundedSender<CapturedRequest>,
            >,
            request: axum::extract::Request,
        ) -> axum::Json<serde_json::Value> {
            let (parts, _) = request.into_parts();
            sender.send((parts.uri, parts.headers)).unwrap();
            axum::Json(serde_json::json!({ "data": [{ "id": "model-a" }] }))
        }

        let (sender, mut requests) = tokio::sync::mpsc::unbounded_channel();
        let app = axum::Router::new()
            .route("/custom/models", axum::routing::get(models))
            .with_state(sender);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let directory = tempfile::tempdir().unwrap();
        let store = Store::connect(&format!(
            "sqlite://{}",
            directory.path().join("redacted-discovery.db").display()
        ))
        .await
        .unwrap();
        let service = service_for(store.clone());
        let base_url = format!("http://{address}/custom/responses");
        let created = store
            .create_model(&crate::model::ModelConfigInput {
                model_id: "model-a".into(),
                display_name: "Model A".into(),
                group_name: None,
                model_type: ModelType::OpenAi,
                base_url: base_url.clone(),
                use_full_url: true,
                api_key: "stored-key".into(),
                tooltip_data: "Model A".into(),
                sort_order: 0,
                default_context: None,
                default_effort: None,
                effort_options: Vec::new(),
                context_options: Vec::new(),
                openai_endpoint: "/v1/responses".into(),
                openai_extra_params_enabled: false,
                openai_extra_params: serde_json::json!({}),
                custom_headers_enabled: true,
                custom_headers: serde_json::json!({
                    "X-Api-Key": "stored-header-key",
                    "X-Tenant": "tenant-a"
                }),
                anthropic_extra_params_enabled: false,
                anthropic_extra_params: serde_json::json!({}),
                context_window_tokens: None,
                max_completion_tokens: None,
                anthropic_max_tokens: None,
                thinking_budget_tokens: None,
            })
            .await
            .unwrap();

        let result = service
            .discover_models(&super::ModelDiscoveryInput {
                model_type: ModelType::OpenAi,
                base_url,
                api_key: crate::model::REDACTED_SECRET.into(),
                model_hash: Some(created.model_hash.clone()),
                custom_headers_enabled: true,
                custom_headers: serde_json::json!({
                    "X-Api-Key": crate::model::REDACTED_SECRET,
                    "X-Tenant": "tenant-a"
                }),
            })
            .await
            .unwrap();

        assert_eq!(result.models, vec!["model-a"]);
        let (uri, headers) = requests.recv().await.unwrap();
        assert_eq!(uri.path(), "/custom/models");
        assert_eq!(
            headers.get(axum::http::header::AUTHORIZATION).unwrap(),
            "Bearer stored-key"
        );
        assert_eq!(headers.get("x-api-key").unwrap(), "stored-header-key");
        assert_eq!(headers.get("x-tenant").unwrap(), "tenant-a");
        server.abort();
    }
}
