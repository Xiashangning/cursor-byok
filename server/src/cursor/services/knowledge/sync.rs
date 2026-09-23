//! Replays the offline journal to upstream and mirrors upstream list state.
use axum::{
    body::{Body, Bytes},
    http::{header, HeaderMap, HeaderValue, Method, Request},
};
use prost::Message;

use crate::{api::cursor::proxy, cursor::protocol::connect, Result};

use super::{
    store::{JournalOp, RuleRecord, RuleStore},
    KnowledgeBaseAddRequest, KnowledgeBaseAddResponse, KnowledgeBaseListItem,
    KnowledgeBaseRemoveRequest, KnowledgeBaseRemoveResponse, KnowledgeBaseUpdateRequest,
    KnowledgeBaseUpdateResponse,
};

const ADD_PATH: &str = "/aiserver.v1.AiService/KnowledgeBaseAdd";
const UPDATE_PATH: &str = "/aiserver.v1.AiService/KnowledgeBaseUpdate";
const REMOVE_PATH: &str = "/aiserver.v1.AiService/KnowledgeBaseRemove";

/// 同一条目被上游明确拒绝的上限;超过即丢弃(死信),避免卡死后续日志。
const MAX_REPLAY_REJECTIONS: u32 = 8;

/// 逐条把离线日志推送到上游。返回 true 表示日志已清空(上游可用),
/// false 表示上游不可达,剩余日志保留、调用方应降级到本地。
pub async fn replay(
    upstream: &proxy::CursorProxy,
    headers: &HeaderMap,
    store: &RuleStore,
) -> Result<bool> {
    while let Some(entry) = store.journal_front()? {
        let outcome = match entry.op {
            JournalOp::Add => replay_add(upstream, headers, store, &entry.id).await?,
            JournalOp::Update => replay_update(upstream, headers, store, &entry.id).await?,
            JournalOp::Remove => replay_remove(upstream, headers, store, &entry.id).await?,
        };
        match outcome {
            ReplayOutcome::Advanced => {}
            ReplayOutcome::Unreachable => return Ok(false),
            ReplayOutcome::Rejected => {
                let attempts = store.record_rejection()?;
                if attempts < MAX_REPLAY_REJECTIONS {
                    tracing::warn!(
                        id = entry.id,
                        op = ?entry.op,
                        attempts,
                        "rules upstream declined replayed entry; retaining journal entry"
                    );
                    return Ok(false);
                }
                tracing::error!(
                    id = entry.id,
                    op = ?entry.op,
                    attempts,
                    "rules journal entry permanently rejected by upstream; dropping it"
                );
                store.pop_journal()?;
            }
        }
    }
    Ok(true)
}

/// 用上游返回的完整列表覆盖本地镜像。仅应在日志已清空时调用。
pub fn mirror(store: &RuleStore, items: Vec<KnowledgeBaseListItem>) -> Result<()> {
    let records = items
        .into_iter()
        .map(|item| RuleRecord {
            id: item.id,
            knowledge: item.knowledge,
            title: item.title,
            created_at: item.created_at,
            is_generated: item.is_generated,
            git_origin: String::new(),
        })
        .collect::<Vec<_>>();
    store.replace_all(&records)
}

/// 单条日志的回放结果。
enum ReplayOutcome {
    /// 已同步,日志弹出,继续回放后续条目。
    Advanced,
    /// 上游不可达或响应不可读:保留日志,本轮回放到此为止。
    Unreachable,
    /// 上游明确拒绝:计入拒绝次数,达到上限后丢弃。
    Rejected,
}

async fn replay_add(
    upstream: &proxy::CursorProxy,
    headers: &HeaderMap,
    store: &RuleStore,
    id: &str,
) -> Result<ReplayOutcome> {
    let Some(record) = store.get(id)? else {
        // 规则文件已不在(被手动删除等),日志作废。
        store.pop_journal()?;
        return Ok(ReplayOutcome::Advanced);
    };
    let message = KnowledgeBaseAddRequest {
        knowledge: record.knowledge,
        title: record.title,
        git_origin: record.git_origin,
        composer_id: None,
    };
    let body = match send(upstream, headers, ADD_PATH, &message).await {
        SendOutcome::Ok(body) => body,
        SendOutcome::Rejected => return Ok(ReplayOutcome::Rejected),
        SendOutcome::Unreachable => return Ok(ReplayOutcome::Unreachable),
    };
    let Ok(reply) = connect::decode_unary::<KnowledgeBaseAddResponse>(&body) else {
        return Ok(ReplayOutcome::Unreachable);
    };
    if !reply.success || reply.id.is_empty() {
        return Ok(ReplayOutcome::Rejected);
    }
    store.promote_and_pop(id, &reply.id)?;
    tracing::info!(
        local_id = id,
        upstream_id = reply.id,
        "replayed offline rule add to upstream"
    );
    Ok(ReplayOutcome::Advanced)
}

async fn replay_update(
    upstream: &proxy::CursorProxy,
    headers: &HeaderMap,
    store: &RuleStore,
    id: &str,
) -> Result<ReplayOutcome> {
    let Some(record) = store.get(id)? else {
        store.pop_journal()?;
        return Ok(ReplayOutcome::Advanced);
    };
    let message = KnowledgeBaseUpdateRequest {
        id: id.into(),
        knowledge: record.knowledge,
        title: record.title,
    };
    let body = match send(upstream, headers, UPDATE_PATH, &message).await {
        SendOutcome::Ok(body) => body,
        SendOutcome::Rejected => return Ok(ReplayOutcome::Rejected),
        SendOutcome::Unreachable => return Ok(ReplayOutcome::Unreachable),
    };
    let Ok(reply) = connect::decode_unary::<KnowledgeBaseUpdateResponse>(&body) else {
        return Ok(ReplayOutcome::Unreachable);
    };
    if !reply.success {
        return Ok(ReplayOutcome::Rejected);
    }
    store.pop_journal()?;
    Ok(ReplayOutcome::Advanced)
}

async fn replay_remove(
    upstream: &proxy::CursorProxy,
    headers: &HeaderMap,
    store: &RuleStore,
    id: &str,
) -> Result<ReplayOutcome> {
    let message = KnowledgeBaseRemoveRequest { id: id.into() };
    let body = match send(upstream, headers, REMOVE_PATH, &message).await {
        SendOutcome::Ok(body) => body,
        SendOutcome::Rejected => return Ok(ReplayOutcome::Rejected),
        SendOutcome::Unreachable => return Ok(ReplayOutcome::Unreachable),
    };
    let Ok(reply) = connect::decode_unary::<KnowledgeBaseRemoveResponse>(&body) else {
        return Ok(ReplayOutcome::Unreachable);
    };
    if !reply.success {
        return Ok(ReplayOutcome::Rejected);
    }
    store.pop_journal()?;
    Ok(ReplayOutcome::Advanced)
}

enum SendOutcome {
    Ok(Bytes),
    Rejected,
    Unreachable,
}

/// 以当前请求的头为模板向上游发起一次 unary RPC。
async fn send(
    upstream: &proxy::CursorProxy,
    template: &HeaderMap,
    path: &str,
    message: &impl Message,
) -> SendOutcome {
    let mut headers = template.clone();
    // 模板里的上游 URL 头指向原始 RPC 路径,必须移除才能命中回放路径。
    headers.remove(proxy::UPSTREAM_URL_HEADER);
    headers.remove(header::CONTENT_LENGTH);
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/proto"),
    );
    let mut request = Request::new(Body::from(message.encode_to_vec()));
    *request.method_mut() = Method::POST;
    *request.uri_mut() = path.parse().expect("replay path is a valid URI");
    *request.headers_mut() = headers;

    match proxy::forward_buffered(upstream, request).await {
        Ok(response) if response.status.is_success() => SendOutcome::Ok(response.body),
        Ok(response) => {
            tracing::warn!(path, status = %response.status, "rules journal replay rejected by upstream");
            SendOutcome::Rejected
        }
        Err(error) => {
            tracing::warn!(path, %error, "rules journal replay cannot reach upstream");
            SendOutcome::Unreachable
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{network::NetworkClients, store::Store};
    use axum::{http::Response, routing::post, Router};

    async fn proxy_with_reply(path: &'static str, body: Vec<u8>) -> proxy::CursorProxy {
        let app = Router::new().route(
            path,
            post(move || {
                let body = body.clone();
                async move { Response::new(Body::from(body)) }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let store = Store::connect("sqlite::memory:").await.unwrap();
        proxy::CursorProxy::for_test(NetworkClients::new(store), format!("http://{address}"))
    }

    fn record(id: &str) -> RuleRecord {
        RuleRecord {
            id: id.into(),
            knowledge: "knowledge".into(),
            title: "title".into(),
            created_at: "2026-01-01T00:00:00.000Z".into(),
            is_generated: false,
            git_origin: String::new(),
        }
    }

    #[tokio::test]
    async fn rejected_add_is_retained() {
        let root = tempfile::tempdir().unwrap();
        let store = RuleStore::open(root.path().join("rules")).unwrap();
        store.upsert_and_record_add(&record("local-a")).unwrap();
        let upstream = proxy_with_reply(
            ADD_PATH,
            KnowledgeBaseAddResponse {
                success: false,
                id: String::new(),
            }
            .encode_to_vec(),
        )
        .await;

        assert!(!replay(&upstream, &HeaderMap::new(), &store).await.unwrap());
        let front = store.journal_front().unwrap().unwrap();
        assert_eq!(front.op, JournalOp::Add);
        assert_eq!(front.attempts, 1);
    }

    #[tokio::test]
    async fn rejected_update_is_retained() {
        let root = tempfile::tempdir().unwrap();
        let store = RuleStore::open(root.path().join("rules")).unwrap();
        store.upsert_and_record_update(&record("42")).unwrap();
        let upstream = proxy_with_reply(
            UPDATE_PATH,
            KnowledgeBaseUpdateResponse { success: false }.encode_to_vec(),
        )
        .await;

        assert!(!replay(&upstream, &HeaderMap::new(), &store).await.unwrap());
        assert_eq!(
            store.journal_front().unwrap().unwrap().op,
            JournalOp::Update
        );
    }

    #[tokio::test]
    async fn rejected_remove_is_retained() {
        let root = tempfile::tempdir().unwrap();
        let store = RuleStore::open(root.path().join("rules")).unwrap();
        store.upsert(&record("42")).unwrap();
        store.remove_and_record("42").unwrap();
        let upstream = proxy_with_reply(
            REMOVE_PATH,
            KnowledgeBaseRemoveResponse { success: false }.encode_to_vec(),
        )
        .await;

        assert!(!replay(&upstream, &HeaderMap::new(), &store).await.unwrap());
        assert_eq!(
            store.journal_front().unwrap().unwrap().op,
            JournalOp::Remove
        );
    }

    #[tokio::test]
    async fn permanently_rejected_entry_is_dropped_after_the_retry_cap() {
        let root = tempfile::tempdir().unwrap();
        let store = RuleStore::open(root.path().join("rules")).unwrap();
        store.upsert_and_record_add(&record("local-a")).unwrap();
        store.upsert_and_record_add(&record("local-b")).unwrap();
        let add_rejection = proxy_with_reply(
            ADD_PATH,
            KnowledgeBaseAddResponse {
                success: false,
                id: String::new(),
            }
            .encode_to_vec(),
        )
        .await;

        for attempt in 1..MAX_REPLAY_REJECTIONS {
            assert!(!replay(&add_rejection, &HeaderMap::new(), &store)
                .await
                .unwrap());
            assert_eq!(
                store.journal_front().unwrap().unwrap().attempts,
                attempt,
                "attempt {attempt} must be retained"
            );
        }
        // 达到上限:队头条目被丢弃,回放继续处理后续条目。
        assert!(!replay(&add_rejection, &HeaderMap::new(), &store)
            .await
            .unwrap());
        let front = store.journal_front().unwrap().unwrap();
        assert_eq!(front.id, "local-b");
        assert_eq!(front.attempts, 1);
    }
}
