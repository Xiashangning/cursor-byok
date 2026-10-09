//! Rewritten BidiAppend forwarding keeps Content-Length consistent with the body.
mod support;

use std::{sync::Arc, time::Duration};

use axum::{
    body::{Body, Bytes},
    extract::State,
    http::{header, Request, StatusCode},
    routing::post,
    Router,
};
use cursor_server::{
    api::cursor::{bidi::AppendEncoding, proxy::CursorProxy, router_with_proxy},
    cursor::protocol::{
        connect,
        proto::{agent::v1 as pb, aiserver::v1 as ai},
    },
    model::ModelSelection,
    network::NetworkClients,
};
use prost::Message;
use support::{openai_model_input, prompt_assets, temp_store, FakeProvider};
use tokio::sync::Mutex;
use tower::ServiceExt;

const APPEND: &str = "/aiserver.v1.BidiService/BidiAppend";

#[derive(Default)]
struct Captured {
    /// (Content-Length header value, actual body bytes received).
    requests: Vec<(Option<usize>, Bytes)>,
}

impl Captured {
    /// 已声明的 Content-Length 必须与真实 body 字节一致(分块转发未声明
    /// 长度也合法)。
    fn assert_length_matches_body(&self, index: usize) {
        let (declared, body) = &self.requests[index];
        if let Some(declared) = declared {
            assert_eq!(
                *declared,
                body.len(),
                "forwarded Content-Length must equal the actual body length"
            );
        }
    }

    /// 重编码路径必须携带精确 Content-Length:修复前沿用客户端过期头,
    /// hyper 对已知长度头优先,变长截断/变短中止。
    fn assert_exact_length_declared(&self, index: usize) {
        let (declared, body) = &self.requests[index];
        assert_eq!(
            *declared,
            Some(body.len()),
            "re-encoded forwarding must declare the exact new body length"
        );
    }
}

/// 本地捕获上游:记录收到的 Content-Length 头与真实 body 字节,
/// 回一个合法的空 BidiAppendResponse。
async fn capture_upstream() -> (String, Arc<Mutex<Captured>>) {
    let captured = Arc::new(Mutex::new(Captured::default()));
    let state = captured.clone();
    let app = Router::new().route(
        APPEND,
        post(
            move |State(state): State<Arc<Mutex<Captured>>>, request: Request<Body>| async move {
                let declared = request
                    .headers()
                    .get(header::CONTENT_LENGTH)
                    .and_then(|value| value.to_str().ok())
                    .and_then(|value| value.parse::<usize>().ok());
                let body = axum::body::to_bytes(request.into_body(), 64 * 1024 * 1024)
                    .await
                    .unwrap();
                state.lock().await.requests.push((declared, body));
                connect::proto_response(&ai::BidiAppendResponse {})
            },
        )
        .with_state(state),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{address}"), captured)
}

async fn forwarding_router(
    upstream: &str,
) -> (tempfile::TempDir, cursor_server::store::Store, Router) {
    let (directory, store) = temp_store().await;
    let provider = FakeProvider::default();
    let registry = cursor_server::cursor::TransportRegistry::new(
        store.clone(),
        Arc::new(provider),
        cursor_server::cursor::prompting::PromptCompiler::new(prompt_assets()),
    );
    let proxy = CursorProxy::for_test(NetworkClients::new(store.clone()), upstream.to_owned());
    let knowledge = cursor_server::cursor::services::knowledge::KnowledgeService::with_root(
        directory.path().join("rules"),
    )
    .unwrap();
    let router = router_with_proxy(registry, proxy, knowledge);
    (directory, store, router)
}

/// Save an Official model selection for the subagent conversation so the
/// resume append is rewritten (`saved` path in resolve_model_selection).
/// store::set_conversation_model_selection is crate-private; the column
/// holds the serialized selection, so the test writes the same JSON shape.
async fn save_official_selection(store: &cursor_server::store::Store, model: &str) {
    let id = cursor_server::model::ConversationId::new("resume-conversation");
    store.ensure_conversation(&id).await.unwrap();
    let selection = serde_json::to_string(&ModelSelection::Official {
        model: model.into(),
        parameters: Default::default(),
    })
    .unwrap();
    sqlx::query("UPDATE conversations SET model_selection = ? WHERE conversation_id = ?")
        .bind(&selection)
        .bind(id.as_str())
        .execute(store.pool())
        .await
        .unwrap();
}

fn resume_append(encoding: AppendEncoding, stale_client_model: &str, extra_bytes: usize) -> Bytes {
    let message = pb::AgentClientMessage {
        message: Some(pb::agent_client_message::Message::RunRequest(
            pb::AgentRunRequest {
                conversation_id: Some("resume-conversation".into()),
                subagent_type_name: Some("generalPurpose".into()),
                requested_model: Some(pb::RequestedModel {
                    model_id: stale_client_model.into(),
                    // Padding drives the rewritten body length relative to the
                    // stale client value: grows/shrinks/unchanged.
                    parameters: vec![pb::requested_model::ModelParameterValue {
                        id: "thinking".into(),
                        value: "x".repeat(extra_bytes),
                    }],
                    ..Default::default()
                }),
                ..Default::default()
            },
        )),
    };
    let mut append = ai::BidiAppendRequest {
        request_id: Some(ai::BidiRequestId {
            request_id: "resume-request".into(),
        }),
        append_seqno: 0,
        data: String::new(),
        data_binary: Vec::new(),
    };
    match encoding {
        AppendEncoding::Hex => append.data = hex::encode(message.encode_to_vec()),
        AppendEncoding::Binary => append.data_binary = message.encode_to_vec(),
    }
    connect::encode_message(&append).unwrap()
}

fn post_append(body: Bytes) -> Request<Body> {
    Request::post(APPEND)
        .header(header::CONTENT_TYPE, "application/proto")
        .header(header::CONTENT_LENGTH, body.len())
        .body(Body::from(body))
        .unwrap()
}

async fn forward(router: &Router, body: Bytes) -> (StatusCode, Bytes) {
    let response = tokio::time::timeout(
        Duration::from_secs(10),
        router.clone().oneshot(post_append(body)),
    )
    .await
    .unwrap()
    .unwrap();
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), 4096)
        .await
        .unwrap();
    (status, body)
}

#[tokio::test]
async fn conflicting_initial_routes_return_a_protocol_error_without_forwarding() {
    let (upstream, captured) = capture_upstream().await;
    let (directory, store) = temp_store().await;
    let registry = cursor_server::cursor::TransportRegistry::new(
        store.clone(),
        Arc::new(FakeProvider::default()),
        cursor_server::cursor::prompting::PromptCompiler::new(prompt_assets()),
    );
    let local = registry.get_or_create("resume-request").await.unwrap();
    let proxy = CursorProxy::for_test(NetworkClients::new(store.clone()), upstream);
    let knowledge = cursor_server::cursor::services::knowledge::KnowledgeService::with_root(
        directory.path().join("rules"),
    )
    .unwrap();
    let router = router_with_proxy(registry.clone(), proxy, knowledge);
    save_official_selection(&store, "official-saved-model").await;
    let (status, _) = forward(&router, resume_append(AppendEncoding::Hex, "stale", 0)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(captured.lock().await.requests.is_empty());
    assert!(!registry.upstream("resume-request").await);
    assert!(local.subscribe().is_some());
    registry.shutdown().await;
}

#[tokio::test]
async fn rewritten_append_with_growing_body_forwards_a_matching_content_length() {
    // 短客户端模型 id 被较长的保存选择替换:重编码 body 比原始 body 长。
    exercise_rewritten_forward(AppendEncoding::Hex, "s", 400, ForwardedDelta::Grows).await;
}

#[tokio::test]
async fn rewritten_append_with_shrinking_body_forwards_a_matching_content_length() {
    // 长客户端模型 id 被较短的保存选择替换:重编码 body 比原始 body 短。
    exercise_rewritten_forward(
        AppendEncoding::Hex,
        "a-much-longer-stale-client-model-identifier-value",
        400,
        ForwardedDelta::Shrinks,
    )
    .await;
}

#[tokio::test]
async fn rewritten_append_with_equal_length_body_forwards_a_matching_content_length() {
    exercise_rewritten_forward(
        AppendEncoding::Hex,
        "official-saved-model",
        2_000,
        ForwardedDelta::Equal,
    )
    .await;
}

#[tokio::test]
async fn binary_rewritten_append_forwards_in_the_binary_representation() {
    exercise_rewritten_forward(AppendEncoding::Binary, "s", 400, ForwardedDelta::Grows).await;
}

#[derive(Clone, Copy, PartialEq)]
enum ForwardedDelta {
    Grows,
    Shrinks,
    Equal,
}

/// Rewritten + upstream: the forwarded Content-Length must equal the actual
/// body bytes in every case, and the bytes must decode to the rewritten
/// message (saved model selection applied) in the client's representation.
async fn exercise_rewritten_forward(
    encoding: AppendEncoding,
    stale_client_model: &str,
    stale_bytes: usize,
    delta: ForwardedDelta,
) {
    let (upstream, captured) = capture_upstream().await;
    let (_directory, store, router) = forwarding_router(&upstream).await;
    save_official_selection(&store, "official-saved-model").await;
    let body = resume_append(encoding, stale_client_model, stale_bytes);

    let (status, error_body) = forward(&router, body.clone()).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&error_body)
    );

    let captured = captured.lock().await;
    assert_eq!(captured.requests.len(), 1);
    captured.assert_exact_length_declared(0);
    let forwarded = captured.requests[0].1.clone();
    // 重编码确实改变了 body 长度(或确认等长):CL 若沿用客户端原值必错。
    match delta {
        ForwardedDelta::Grows => assert!(
            forwarded.len() > body.len(),
            "grow case must actually grow: {} vs {}",
            forwarded.len(),
            body.len()
        ),
        ForwardedDelta::Shrinks => assert!(
            forwarded.len() < body.len(),
            "shrink case must actually shrink: {} vs {}",
            forwarded.len(),
            body.len()
        ),
        ForwardedDelta::Equal => assert_eq!(
            forwarded.len(),
            body.len(),
            "equal case must stay the same length"
        ),
    }
    let append: ai::BidiAppendRequest = connect::decode_unary(&forwarded).unwrap();
    match encoding {
        AppendEncoding::Hex => {
            assert!(!append.data.is_empty());
            assert!(append.data_binary.is_empty());
        }
        AppendEncoding::Binary => {
            assert!(append.data.is_empty());
            assert!(!append.data_binary.is_empty());
        }
    }
    let payload = match encoding {
        AppendEncoding::Hex => hex::decode(&append.data).unwrap(),
        AppendEncoding::Binary => append.data_binary,
    };
    let message = pb::AgentClientMessage::decode(payload.as_slice()).unwrap();
    let pb::agent_client_message::Message::RunRequest(run) = message.message.unwrap() else {
        panic!("expected rewritten RunRequest")
    };
    // The saved Official selection replaced the stale client value.
    assert_eq!(
        run.requested_model.as_ref().unwrap().model_id,
        "official-saved-model"
    );
}

/// Un-rewritten forwarding is byte-identical: same body bytes, and the
/// Content-Length still matches (it was never stale).
#[tokio::test]
async fn unrewritten_append_forwards_the_original_bytes_unchanged() {
    let (upstream, captured) = capture_upstream().await;
    let (_directory, _store, router) = forwarding_router(&upstream).await;
    // No saved selection and a subagent-less root RunRequest: unmatched model
    // routes upstream without rewriting.
    let message = pb::AgentClientMessage {
        message: Some(pb::agent_client_message::Message::RunRequest(
            pb::AgentRunRequest {
                conversation_id: Some("root-conversation".into()),
                requested_model: Some(pb::RequestedModel {
                    model_id: "official-unknown-model".into(),
                    ..Default::default()
                }),
                ..Default::default()
            },
        )),
    };
    let append = ai::BidiAppendRequest {
        request_id: Some(ai::BidiRequestId {
            request_id: "root-request".into(),
        }),
        append_seqno: 0,
        data: hex::encode(message.encode_to_vec()),
        data_binary: Vec::new(),
    };
    let body = connect::encode_message(&append).unwrap();

    let (status, error_body) = forward(&router, body.clone()).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&error_body)
    );

    let captured = captured.lock().await;
    assert_eq!(captured.requests.len(), 1);
    captured.assert_length_matches_body(0);
    assert_eq!(
        captured.requests[0].1, body,
        "un-rewritten forwarding must be byte-identical"
    );
}

/// B-1 复现核心:重编码后的 body 长度变化时,若沿用客户端原始
/// Content-Length,hyper 会在变长时静默截断、变短时中止。测试断言捕获
/// 上游收到的声明长度与真实字节一致,即转发层已换成新 body 的精确长度。
#[tokio::test]
async fn rewritten_append_never_forwards_a_stale_content_length() {
    let (upstream, captured) = capture_upstream().await;
    let (_directory, store, router) = forwarding_router(&upstream).await;
    save_official_selection(&store, "official-saved-model").await;
    let body = resume_append(AppendEncoding::Hex, "s", 8_000);

    let (status, error_body) = forward(&router, body).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&error_body)
    );

    let captured = captured.lock().await;
    assert_eq!(captured.requests.len(), 1);
    captured.assert_exact_length_declared(0);
    // 重编码后的 body 必须能完整解出重写的消息——被旧 CL 截断的 body 做不到。
    let forwarded = captured.requests[0].1.clone();
    let append: ai::BidiAppendRequest = connect::decode_unary(&forwarded).unwrap();
    let message =
        pb::AgentClientMessage::decode(hex::decode(&append.data).unwrap().as_slice()).unwrap();
    let pb::agent_client_message::Message::RunRequest(run) = message.message.unwrap() else {
        panic!("expected rewritten RunRequest")
    };
    assert_eq!(
        run.requested_model.as_ref().unwrap().model_id,
        "official-saved-model"
    );
}

/// B-2:binary 编码的 append 走完整本地 HTTP 路由:解码、模型选择、
/// 本地 transport 建立全部可用(不转发上游——本地模型路由)。
#[tokio::test]
async fn binary_append_decodes_and_routes_locally() {
    let (upstream, _captured) = capture_upstream().await;
    let (_directory, store, router) = forwarding_router(&upstream).await;
    store
        .create_model(&openai_model_input("local-model", None))
        .await
        .unwrap();
    // 模型回复由 FakeProvider 提供不了(router 用独立 store 构造 registry);
    // 这里只验证 seqno=0 建立本地 transport 并返回 200,不跑完整轮次。
    let message = pb::AgentClientMessage {
        message: Some(pb::agent_client_message::Message::RunRequest(
            pb::AgentRunRequest {
                conversation_id: Some("binary-local-conversation".into()),
                requested_model: Some(pb::RequestedModel {
                    model_id: "local-model".into(),
                    ..Default::default()
                }),
                ..Default::default()
            },
        )),
    };
    let append = ai::BidiAppendRequest {
        request_id: Some(ai::BidiRequestId {
            request_id: "binary-local".into(),
        }),
        append_seqno: 0,
        data: String::new(),
        data_binary: message.encode_to_vec(),
    };
    let body = connect::encode_message(&append).unwrap();

    let (status, error_body) = forward(&router, body).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&error_body)
    );
    // 上游捕获零请求:binary append 走本地路由,不经转发。
    assert!(
        _captured.lock().await.requests.is_empty(),
        "the local route must not forward upstream"
    );
}

/// B-2:hex 与 binary 双载荷的歧义请求被明确拒绝。
#[tokio::test]
async fn dual_payload_append_is_rejected() {
    let message = pb::AgentClientMessage {
        message: Some(pb::agent_client_message::Message::ClientHeartbeat(
            Default::default(),
        )),
    };
    let request = ai::BidiAppendRequest {
        request_id: Some(ai::BidiRequestId {
            request_id: "dual-payload".into(),
        }),
        append_seqno: 0,
        data: hex::encode(message.encode_to_vec()),
        data_binary: message.encode_to_vec(),
    };
    let error = match cursor_server::api::cursor::bidi::decode(&request) {
        Err(error) => error,
        Ok(_) => panic!("dual-payload append must be rejected"),
    };
    assert!(error.to_string().contains("both data and data_binary"));
}

/// B-4/V-11:改写路径不再累积同 id 空值占位。变体命中路径把空占位折叠
/// 为单条;续接路径按保存选择定向覆盖,未知非空参数原样保留。
#[tokio::test]
async fn rewritten_parameters_collapse_duplicate_empty_placeholders() {
    let (_directory, store) = temp_store().await;
    let provider = FakeProvider::default();
    let registry = cursor_server::cursor::TransportRegistry::new(
        store.clone(),
        Arc::new(provider),
        cursor_server::cursor::prompting::PromptCompiler::new(prompt_assets()),
    );
    // 变体命中路径:本地模型 + 上下文/effort 轴。
    let mut input = openai_model_input("dedup-model", None);
    input.effort_options = vec!["low".into(), "high".into()];
    input.context_options = vec!["200k".into()];
    let model = store.create_model(&input).await.unwrap();

    // 客户端回程携带两个 context 空值占位与一个 effort 空值占位加非空 thinking。
    let parameters = || {
        vec![
            pb::requested_model::ModelParameterValue {
                id: "context".into(),
                value: String::new(),
            },
            pb::requested_model::ModelParameterValue {
                id: "context".into(),
                value: String::new(),
            },
            pb::requested_model::ModelParameterValue {
                id: "effort".into(),
                value: String::new(),
            },
            pb::requested_model::ModelParameterValue {
                id: "thinking".into(),
                value: "true".into(),
            },
        ]
    };
    let mut decoded = cursor_server::api::cursor::bidi::DecodedAppend {
        request_id: "dedup-request".into(),
        seqno: 0,
        rewritten: false,
        encoding: AppendEncoding::Hex,
        message: pb::AgentClientMessage {
            message: Some(pb::agent_client_message::Message::RunRequest(
                pb::AgentRunRequest {
                    requested_model: Some(pb::RequestedModel {
                        model_id: format!("{}-200k-high", model.model_hash),
                        parameters: parameters(),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
            )),
        },
    };
    decoded.resolve_model_selection(&registry).await.unwrap();
    assert!(decoded.rewritten, "variant hit must rewrite the message");
    let pb::agent_client_message::Message::RunRequest(run) = decoded.message.message.unwrap()
    else {
        panic!()
    };
    let parameters = &run.requested_model.unwrap().parameters;
    // 空占位折叠:context 空值只剩一条,effort 空值被命中的变体值取代。
    assert_eq!(
        parameters
            .iter()
            .filter(|parameter| parameter.id == "context" && parameter.value.is_empty())
            .count(),
        1,
        "duplicate empty context placeholders must collapse: {parameters:?}"
    );
    assert_eq!(
        parameters
            .iter()
            .filter(|parameter| parameter.id == "context" && parameter.value == "200k")
            .count(),
        1,
        "the variant context value must be applied once: {parameters:?}"
    );
    // 空值 effort 占位:非子代理路径不做已知 id 过滤,占位保留一条
    // (下游消费方跳过空值);命中的变体值经 reasoning 下发。
    assert_eq!(
        parameters
            .iter()
            .filter(|parameter| parameter.id == "effort" && parameter.value.is_empty())
            .count(),
        1,
        "at most one empty effort placeholder may remain: {parameters:?}"
    );
    assert_eq!(
        parameters
            .iter()
            .filter(|parameter| parameter.id == "reasoning" && parameter.value == "high")
            .count(),
        1,
        "the variant effort value arrives via reasoning: {parameters:?}"
    );
    // 非空 thinking 保留且不重复。
    assert_eq!(
        parameters
            .iter()
            .filter(|parameter| parameter.id == "thinking" && parameter.value == "true")
            .count(),
        1,
        "non-empty unknown parameters must survive exactly once: {parameters:?}"
    );
}
