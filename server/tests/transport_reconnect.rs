//! Transport lifecycle: reconnects create fresh transports; disconnect cleans up.
mod support;

use std::time::Duration;

use cursor_server::cursor::TransportCommand;
use support::{
    drive, registry, run_request, temp_store, text_response, user_message_action, FakeProvider,
};

/// L-4 回归:断流重连(新 request_id)创建全新 transport——新 inbox 从 0
/// 重新开始,不延续旧 transport 的任何状态;旧 transport 断开后被清理。
#[tokio::test]
async fn reconnect_with_a_new_request_id_builds_a_fresh_transport() {
    let (_directory, store) = temp_store().await;
    let provider = FakeProvider::default();
    // 第一个流:模型回复后正常结束;断连重连后第二个流重新激活模型。
    provider.push(text_response("first-stream", "first run done"));
    provider.push(text_response("second-stream", "second run done"));
    let registry = registry(store.clone(), provider.clone());

    // 第一次连接:下发 RunRequest 后断开(断流),该运行被取消。
    let first = registry.get_or_create("reconnect-run").await.unwrap();
    assert!(first.subscribe().is_some());
    first
        .command(TransportCommand::Append {
            seqno: 0,
            message: Box::new(run_request(
                "reconnect-conversation",
                "reconnect-run",
                "test-model",
                None,
                user_message_action("first", "user-1", None),
            )),
        })
        .await
        .unwrap();
    // 留一个序号空洞,验证断连清理不依赖后续消息已被执行。
    first.command(TransportCommand::Append {
        seqno: 2,
        message: Box::new(cursor_server::cursor::protocol::proto::agent::v1::AgentClientMessage {
            message: Some(cursor_server::cursor::protocol::proto::agent::v1::agent_client_message::Message::ClientHeartbeat(Default::default())),
        }),
    }).await.unwrap();
    first.disconnect().await;
    assert!(
        tokio::time::timeout(Duration::from_secs(5), first.wait_transport_closed())
            .await
            .is_ok(),
        "the disconnected transport must close"
    );

    // 客户端重连:新 attempt 携带新 request_id,registry 必须给出全新
    // transport(新 inbox、新输出 hub),而不是复用旧的。
    let second = registry.get_or_create("reconnect-run-2").await.unwrap();
    assert_ne!(
        first.request_id(),
        second.request_id(),
        "a new request id must map to a new transport"
    );
    let mut output = second
        .subscribe()
        .expect("fresh transport accepts subscribers");
    // 新 inbox 从 seqno 0 开始(旧 transport 的序号窗口不延续)。
    second
        .command(TransportCommand::Append {
            seqno: 0,
            message: Box::new(run_request(
                "reconnect-conversation",
                "reconnect-run-2",
                "test-model",
                None,
                user_message_action("second", "user-2", None),
            )),
        })
        .await
        .unwrap();

    let mut seqno = 1;
    let out = drive(&second, &mut output, &mut seqno, |_| vec![]).await;
    assert_eq!(out.terminal, serde_json::json!({}));
    assert_eq!(
        provider.requests().len(),
        1,
        "the disconnected run is cancelled; only the reconnect activates the model"
    );

    registry.shutdown().await;
}

/// L-4 回归:transport 关闭后,registry 的 local 映射条目被异步清理,
/// 同名 request_id 的下一次 append 得到全新 transport。
#[tokio::test]
async fn a_closed_transport_is_removed_from_the_registry() {
    let (_directory, store) = temp_store().await;
    let provider = FakeProvider::default();
    provider.push(text_response("recycled-run", "recycled done"));
    let registry = registry(store, provider.clone());

    let first = registry.get_or_create("recycled-id").await.unwrap();
    first.disconnect().await;
    assert!(
        tokio::time::timeout(Duration::from_secs(5), first.wait_transport_closed())
            .await
            .is_ok()
    );

    // registry 的异步清理(spawn 移除)完成前等待;之后同 id 重新创建。
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while registry.local("recycled-id").await.is_some() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the closed transport must be removed from the registry"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    // 同 id 的全新 transport:接受订阅、跑完整轮次。
    let second = registry.get_or_create("recycled-id").await.unwrap();
    assert!(
        second.subscribe().is_some(),
        "the recreated transport must accept subscribers"
    );
    let mut output = second.subscribe().unwrap();
    second
        .command(TransportCommand::Append {
            seqno: 0,
            message: Box::new(run_request(
                "recycle-conversation",
                "recycled-id",
                "test-model",
                None,
                user_message_action("recycle", "user-1", None),
            )),
        })
        .await
        .unwrap();
    let mut seqno = 1;
    let out = drive(&second, &mut output, &mut seqno, |_| vec![]).await;
    assert_eq!(out.terminal, serde_json::json!({}));

    registry.shutdown().await;
}
