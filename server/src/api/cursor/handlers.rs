//! Implements Cursor HTTP endpoints outside the Agent Run stream.
use prost::Message;
use std::time::Duration;

use axum::{
    body::{to_bytes, Body, Bytes},
    extract::{DefaultBodyLimit, Extension, State},
    http::{header, HeaderMap, HeaderValue, Request, Response, StatusCode},
    routing::{get, post},
    Router,
};
use tower_http::{decompression::RequestDecompressionLayer, limit::RequestBodyLimitLayer};

use crate::{
    api::cursor::{
        bidi,
        proxy::{self, CursorProxy},
        run_sse,
    },
    cursor::{
        protocol::{
            connect, json as protocol_json,
            proto::{agent::v1 as agent, aiserver::v1 as ai},
        },
        services::{
            account, analytics, commit_message, compatibility, entitlement::FreeEntitlementCache,
            knowledge, model_catalog, observability::DecodedMessage, server_config, tab,
        },
        transport::{TransportParent, TransportRegistry, TransportRoute},
    },
    Result,
};

const INITIAL_APPEND_WAIT: Duration = Duration::from_secs(30);

/// 路由判定失败的 trace 行不能伪装成 BYOK:本地路由要求首个消息正向匹配已配置
/// 模型,无法判定时该请求唯一可能的去向是 Cursor 官方上游。若 seqno=0 随后完成
/// 解码,新的 Begin 会把该行重置为真实路由。
const UNRESOLVED_ROUTE_FALLBACK: &str = "cursor_official";
const RUN_REQUEST_LIMIT: usize = 64 * 1024;
const BIDI_REQUEST_LIMIT: usize = 64 * 1024 * 1024;
const DEFAULT_REQUEST_LIMIT: usize = 4 * 1024 * 1024;
const COMPRESSED_REQUEST_LIMIT: usize = 64 * 1024 * 1024;

pub fn router(
    registry: TransportRegistry,
    clients: crate::network::NetworkClients,
) -> Result<Router> {
    let proxy = CursorProxy::cursor(clients);
    let knowledge = knowledge::KnowledgeService::managed()?;
    Ok(router_with_proxy(registry, proxy, knowledge))
}

/// 测试缝:注入指定上游的代理与独立 knowledge 服务(集成测试用本地捕获
/// 上游验证转发字节;生产路径仍走 `router`)。
pub fn router_with_proxy(
    registry: TransportRegistry,
    proxy: CursorProxy,
    knowledge_service: knowledge::KnowledgeService,
) -> Router {
    let web_cache = registry.web_cache().router();
    let free_entitlements = FreeEntitlementCache::default();
    Router::new()
        .route("/__byok-api__/healthz", get(health))
        .route("/agent.v1.AgentService/RunSSE", post(run_sse_handler))
        .route("/aiserver.v1.BidiService/BidiAppend", post(bidi_handler))
        .route(
            "/aiserver.v1.AiService/AvailableDocs",
            post(compatibility::available_docs),
        )
        .route(
            "/aiserver.v1.DashboardService/GetEffectiveUserPlugins",
            post(compatibility::effective_user_plugins),
        )
        .route(
            "/aiserver.v1.DashboardService/GetTeamReposOrEmptyIfNotInTeam",
            post(compatibility::team_configuration),
        )
        .route(
            "/aiserver.v1.DashboardService/GetTeamAdminSettingsOrEmptyIfNotInTeam",
            post(compatibility::team_configuration),
        )
        .route(
            "/aiserver.v1.DashboardService/GetUserPrivacyMode",
            post(compatibility::user_privacy_mode),
        )
        .route(
            "/agent.v1.AgentService/UpdateConversationMetadata",
            post(compatibility::update_conversation_metadata),
        )
        .route(
            "/aiserver.v1.AiService/GetServerConfig",
            post(server_config::get),
        )
        .route(
            "/aiserver.v1.ServerConfigService/GetServerConfig",
            post(server_config::get),
        )
        .route(
            "/aiserver.v1.AiService/AvailableModels",
            post(model_catalog::available_models),
        )
        .route(
            "/agent.v1.AgentService/GetUsableModels",
            post(model_catalog::usable_models),
        )
        .route(
            "/aiserver.v1.AiService/GetUsableModels",
            post(model_catalog::usable_models),
        )
        .route(
            "/aiserver.v1.AiService/WriteGitCommitMessage",
            post(commit_message::write_git_commit_message),
        )
        .route(
            "/aiserver.v1.NetworkService/IsConnected",
            post(is_connected),
        )
        .route(
            "/agent.v1.AgentService/GetDefaultModelForCli",
            post(model_catalog::default_model_for_cli),
        )
        .route(
            "/aiserver.v1.AiService/GetDefaultModelForCli",
            post(model_catalog::default_model_for_cli),
        )
        .route(
            "/aiserver.v1.AiService/GetDefaultModel",
            post(model_catalog::default_model),
        )
        .route(
            "/aiserver.v1.AiService/GetDefaultModelNudgeData",
            post(model_catalog::default_model_nudge),
        )
        .route(
            "/aiserver.v1.AuthService/GetEmail",
            post(account::get_email),
        )
        .route(
            "/aiserver.v1.AuthService/GetUserMeta",
            post(account::get_user_meta),
        )
        .route("/aiserver.v1.DashboardService/GetMe", post(account::get_me))
        .route(
            "/aiserver.v1.DashboardService/GetTeams",
            post(account::get_teams),
        )
        .route(
            "/aiserver.v1.DashboardService/GetUserProfile",
            post(account::get_user_profile),
        )
        .route(
            "/aiserver.v1.DashboardService/GetCurrentPeriodUsage",
            post(account::current_period_usage),
        )
        .route(
            "/aiserver.v1.DashboardService/GetUsageLimitStatusAndActiveGrants",
            post(account::usage_limit_status),
        )
        .route(
            "/aiserver.v1.AiService/KnowledgeBaseAdd",
            post(knowledge::add),
        )
        .route(
            "/aiserver.v1.AiService/KnowledgeBaseList",
            post(knowledge::list),
        )
        .route(
            "/aiserver.v1.AiService/KnowledgeBaseUpdate",
            post(knowledge::update),
        )
        .route(
            "/aiserver.v1.AiService/KnowledgeBaseRemove",
            post(knowledge::remove),
        )
        .route(
            analytics::BOOTSTRAP_STATSIG_PATH,
            post(analytics::bootstrap_statsig),
        )
        .route("/auth/full_stripe_profile", get(account::stripe_profile))
        .route("/auth/stripe_profile", get(account::stripe_profile))
        .merge(tab::router())
        .route_layer(DefaultBodyLimit::max(DEFAULT_REQUEST_LIMIT))
        .route_layer(RequestDecompressionLayer::new())
        // Applied last, so compressed bytes are bounded before decompression.
        .route_layer(RequestBodyLimitLayer::new(COMPRESSED_REQUEST_LIMIT))
        .fallback(proxy::forward)
        .method_not_allowed_fallback(proxy::forward)
        .layer(Extension(proxy))
        .layer(Extension(knowledge_service))
        .layer(Extension(free_entitlements))
        .with_state(registry)
        .merge(web_cache)
}

async fn health() -> StatusCode {
    StatusCode::NO_CONTENT
}

/// `NetworkService/IsConnected` probe. Cursor's always-local extension checks
/// connectivity roughly 10s after any slow request starts; a 404/error here is
/// treated as "network disconnected" and aborts in-flight work (e.g. commit
/// message generation) even while the model is still streaming. Always answer
/// connected with an empty `IsConnectedResponse` so local BYOK generation is
/// never cancelled by this probe.
async fn is_connected() -> Result<Response<Body>> {
    let payload = connect::encode_message(&ai::IsConnectedResponse {})?;
    let mut response = Response::new(Body::from(payload));
    *response.status_mut() = StatusCode::OK;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/proto"),
    );
    Ok(response)
}

async fn run_sse_handler(
    State(registry): State<TransportRegistry>,
    Extension(proxy): Extension<CursorProxy>,
    request: Request<Body>,
) -> Result<Response<Body>> {
    let (parts, body) = buffered(request, RUN_REQUEST_LIMIT).await?;
    let request: agent::BidiRequestId = connect::decode_unary(&body)?;
    let route = registry.wait_route(&request.request_id).await;
    // 订阅可能晚于 trace 行创建;resume 让已存在的 trace 立刻进入记录状态。
    let trace = registry.trace(&request.request_id);
    trace.resume();
    match route {
        crate::cursor::transport::TransportRoute::Local => {
            run_sse::stream(&registry, &request.request_id).await
        }
        crate::cursor::transport::TransportRoute::Upstream(generation) => {
            let response = proxy::forward(
                Extension(proxy),
                Request::from_parts(parts, Body::from(body)),
            )
            .await?;
            Ok(run_sse::upstream(
                registry,
                request.request_id,
                generation,
                response,
                Some(trace),
            )
            .await)
        }
    }
}

async fn bidi_handler(
    State(registry): State<TransportRegistry>,
    Extension(proxy): Extension<CursorProxy>,
    request: Request<Body>,
) -> Result<Response<Body>> {
    let (mut parts, body) = buffered(request, BIDI_REQUEST_LIMIT).await?;
    let request: ai::BidiAppendRequest = connect::decode_unary(&body)?;
    let mut decoded = bidi::decode(&request)?;
    let has_initial_model = decoded.model_id().is_some();
    let conversation_id = decoded.conversation_id().map(str::to_owned);
    let trace_metadata = decoded.trace_metadata();
    let trace = registry.trace(&decoded.request_id);
    // Render before model resolution so `client_message` is the message Cursor
    // actually sent; routing metadata and the raw frame remain adjacent to it.
    let decoded_message = trace
        .is_enabled()
        .then(|| protocol_json::render_client(&decoded.message))
        .flatten()
        .map(|rendered| DecodedMessage {
            data: rendered.json.into(),
            metadata: serde_json::json!({
                "append_seqno": trace_metadata["append_seqno"],
                "message_type": trace_metadata["message_type"],
                "truncated": rendered.truncated,
                "total_bytes": rendered.total_bytes,
            }),
        });
    let selection = decoded.resolve_model_selection(&registry).await?;
    let routed_model = decoded.model_id().map(str::to_owned);
    let local = if let Some(model_id) = decoded.model_id() {
        // 统一目录判定本地路由;未匹配的模型走 Cursor 官方上游。
        if matches!(selection, Some(crate::model::ModelSelection::Local { .. })) {
            tracing::info!(
                request_id = decoded.request_id,
                model_id,
                "routing Cursor Run to BYOK provider"
            );
            true
        } else {
            tracing::info!(
                request_id = decoded.request_id,
                model_id,
                "routing Cursor Run to Cursor upstream"
            );
            false
        }
    } else if registry.local(&decoded.request_id).await.is_some() {
        true
    } else if registry.upstream(&decoded.request_id).await {
        false
    } else if decoded.seqno > 0 {
        // BidiAppend uploads are concurrent. A small heartbeat can arrive before
        // the much larger seqno=0 RunRequest has finished uploading/decoding.
        // Wait for its model-selected route; never guess local vs upstream.
        let route = match tokio::time::timeout(
            INITIAL_APPEND_WAIT,
            registry.wait_route(&decoded.request_id),
        )
        .await
        {
            Ok(route) => route,
            Err(_) => {
                let error = crate::Error::Protocol(
                    "timed out waiting for the initial BidiAppend model selection".into(),
                );
                let message = error.to_string();
                trace.begin(conversation_id.as_deref(), UNRESOLVED_ROUTE_FALLBACK, None);
                trace.bidi_append(
                    body,
                    trace_outcome(
                        trace_metadata,
                        false,
                        "route_timeout",
                        Some(message.clone()),
                    ),
                    decoded_message,
                );
                trace.finish(Some(&message));
                return Err(error);
            }
        };
        matches!(route, TransportRoute::Local)
    } else {
        let error = crate::Error::Protocol("first BidiAppend message must select a model".into());
        let message = error.to_string();
        trace.begin(conversation_id.as_deref(), UNRESOLVED_ROUTE_FALLBACK, None);
        trace.bidi_append(
            body,
            trace_outcome(
                trace_metadata,
                false,
                "missing_transport",
                Some(message.clone()),
            ),
            decoded_message,
        );
        trace.finish(Some(&message));
        return Err(error);
    };
    if has_initial_model {
        trace.begin(
            conversation_id.as_deref(),
            if local {
                "local_byok"
            } else {
                "cursor_official"
            },
            routed_model.as_deref(),
        );
    } else {
        trace.resume();
    }
    if !local {
        if has_initial_model && registry.local(&decoded.request_id).await.is_some() {
            let error = crate::Error::Protocol(
                "BidiAppend cannot change an existing local route to upstream".into(),
            );
            trace.finish(Some(&error.to_string()));
            return Err(error);
        }
        if has_initial_model {
            if let (Some(selection), Some(agent::agent_client_message::Message::RunRequest(run))) =
                (selection.as_ref(), decoded.message.message.as_ref())
            {
                if run.subagent_type_name.is_some() {
                    if let Some(id) = run.conversation_id.as_deref() {
                        let id = crate::model::ConversationId::new(id);
                        registry.store().ensure_conversation(&id).await?;
                        registry
                            .store()
                            .set_conversation_model_selection(&id, Some(selection))
                            .await?;
                    }
                }
            }
            registry.mark_upstream(&decoded.request_id).await;
            if let Some(parent) = parent_headers(&parts.headers)? {
                registry
                    .associate_upstream_task(
                        &decoded.request_id,
                        parent,
                        routed_model.clone().unwrap_or_default(),
                    )
                    .await;
            }
        }
        trace.bidi_append(
            body.clone(),
            trace_outcome(trace_metadata, true, "upstream", None),
            decoded_message.clone(),
        );
        // 仅当路由解析确实改写了消息(名称/变体归一或子代理续接)才重编码;
        // 否则原样转发原始字节——prost 往返会丢弃本仓库 proto 之外的字段。
        // 重编码使用客户端请求的同一表示:hex 请求回 hex,binary 请求回 binary。
        let (forwarded, rewritten) = if decoded.rewritten {
            let mut forwarded = request;
            let encoded = decoded.message.encode_to_vec();
            match decoded.encoding {
                bidi::AppendEncoding::Hex => {
                    forwarded.data = hex::encode(encoded);
                }
                bidi::AppendEncoding::Binary => {
                    forwarded.data_binary = encoded;
                }
            }
            let forwarded = if connect::is_framed_unary(&body) {
                connect::encode_message(&forwarded)?
            } else {
                Bytes::from(forwarded.encode_to_vec())
            };
            (forwarded, true)
        } else {
            (body, false)
        };
        let upstream_generation = match registry.wait_route(&decoded.request_id).await {
            TransportRoute::Upstream(generation) => generation,
            // 防御:同 request_id 先 local 后 official 的重复 seqno=0 不在合法
            // 协议路径内;即便到达也返回协议错误而不是 panic。
            TransportRoute::Local => {
                let error = crate::Error::Protocol(
                    "BidiAppend route resolved to local after upstream forwarding".into(),
                );
                trace.finish(Some(&error.to_string()));
                return Err(error);
            }
        };
        if rewritten {
            // 重编码改变 body 长度;客户端原始 Content-Length 已过期。hyper
            // 对已知长度头优先于流式 body:变长静默截断、变短中止请求。
            // 换成新 body 的精确长度。
            parts
                .headers
                .insert(header::CONTENT_LENGTH, HeaderValue::from(forwarded.len()));
        }
        let response = proxy::forward(
            Extension(proxy),
            Request::from_parts(parts, Body::from(forwarded)),
        )
        .await?;
        return if response.status().is_success() {
            Ok(response)
        } else {
            Ok(run_sse::upstream(
                registry,
                decoded.request_id,
                upstream_generation,
                response,
                Some(trace),
            )
            .await)
        };
    }
    let parent = match parent_headers(&parts.headers) {
        Ok(parent) => parent,
        Err(error) => {
            trace.bidi_append(
                body,
                trace_outcome(
                    trace_metadata,
                    false,
                    "invalid_parent",
                    Some(error.to_string()),
                ),
                decoded_message.clone(),
            );
            return Err(error);
        }
    };
    match bidi::append(&registry, decoded, parent).await {
        Ok(_) => trace.bidi_append(
            body,
            trace_outcome(trace_metadata, true, "local", None),
            decoded_message.clone(),
        ),
        Err(error) => {
            trace.bidi_append(
                body,
                trace_outcome(
                    trace_metadata,
                    false,
                    "command_rejected",
                    Some(error.to_string()),
                ),
                decoded_message.clone(),
            );
            return Err(error);
        }
    }
    Ok(connect::proto_response(&ai::BidiAppendResponse {}))
}

fn trace_outcome(
    mut metadata: serde_json::Value,
    accepted: bool,
    route_outcome: &str,
    error: Option<String>,
) -> serde_json::Value {
    if let Some(metadata) = metadata.as_object_mut() {
        metadata.insert("accepted".into(), accepted.into());
        metadata.insert("route_outcome".into(), route_outcome.into());
        if let Some(error) = error {
            metadata.insert("error".into(), error.into());
        }
    }
    metadata
}

async fn buffered(
    request: Request<Body>,
    decompressed_limit: usize,
) -> Result<(axum::http::request::Parts, Bytes)> {
    let (parts, body) = request.into_parts();
    let body = to_bytes(body, decompressed_limit).await.map_err(|error| {
        // 长度超限映射为 413,与 RequestBodyLimitLayer 对压缩字节上限的处理一致;
        // 其他读取失败仍是 400。
        if is_length_limit(&error) {
            crate::Error::RequestTooLarge(format!(
                "decompressed request body exceeds the {decompressed_limit}-byte limit"
            ))
        } else {
            crate::Error::Protocol(format!("cannot read request body: {error}"))
        }
    })?;
    Ok((parts, body))
}

fn is_length_limit(error: &axum::Error) -> bool {
    std::iter::successors(Some(error as &(dyn std::error::Error + 'static)), |error| {
        error.source()
    })
    .any(|error| error.is::<http_body_util::LengthLimitError>())
}

fn parent_headers(headers: &HeaderMap) -> Result<Option<TransportParent>> {
    let request_id = header_text(headers, "x-parent-request-id")?;
    let tool_call_id = header_text(headers, "x-parent-agent-tool-call-id")?;
    match (request_id, tool_call_id) {
        (None, None) => Ok(None),
        (Some(request_id), Some(tool_call_id)) => Ok(Some(TransportParent {
            request_id: request_id.into(),
            tool_call_id: tool_call_id.into(),
        })),
        _ => Err(crate::Error::Protocol(
            "Cursor subagent request must include both parent headers".into(),
        )),
    }
}

fn header_text<'a>(headers: &'a HeaderMap, name: &str) -> Result<Option<&'a str>> {
    headers
        .get(name)
        .map(|value| value.to_str())
        .transpose()
        .map_err(|error| crate::Error::Protocol(format!("invalid {name} header: {error}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn buffered_requests_reject_decompressed_bodies_over_the_route_limit() {
        use axum::response::IntoResponse;

        let request = Request::new(Body::from(vec![0_u8; 17]));
        let error = buffered(request, 16).await.unwrap_err();
        assert!(matches!(error, crate::Error::RequestTooLarge(_)));
        // 与 RequestBodyLimitLayer 的压缩上限响应一致:413,而不是 400。
        assert_eq!(
            error.into_response().status(),
            StatusCode::PAYLOAD_TOO_LARGE
        );
    }
}
