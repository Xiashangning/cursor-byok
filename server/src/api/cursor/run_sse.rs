//! Subscribes Cursor RunSSE clients to replayable Transport output.
use axum::{
    body::Body,
    http::{header, HeaderValue, Response, StatusCode},
};
use bytes::Bytes;
use std::convert::Infallible;
use tokio::sync::mpsc;
use tokio_stream::StreamExt;

use crate::{
    cursor::{
        protocol::connect::{self, END_STREAM_FLAG},
        services::observability::CursorTraceRecorder,
        transport::{OutputReceiver, TransportHandle, TransportRegistry},
    },
    Result,
};

pub async fn stream(registry: &TransportRegistry, request_id: &str) -> Result<Response<Body>> {
    let handle = registry.get_or_create(request_id).await?;
    let trace = handle.trace().cloned();
    if let Some(trace) = &trace {
        trace.response_started(StatusCode::OK.as_u16());
    }
    let Some(receiver) = handle.subscribe() else {
        return replay_overflow_response(handle, trace);
    };
    let body_stream = local_body_stream(receiver, handle, trace);
    Ok(run_sse_response(Body::from_stream(body_stream)))
}

/// 输出历史超出重放容量后,新订阅者拿不到完整输出。返回带错误的终止帧,
/// 让客户端明确看到失败而不是把静默断流误当成正常运行结束;随后按客户端
/// 断开处理(无其他订阅者时由 runtime 拆除会话,不再空跑)。
fn replay_overflow_response(
    handle: TransportHandle,
    trace: Option<CursorTraceRecorder>,
) -> Result<Response<Body>> {
    let frame = connect::encode_error_end_stream(&connect::ConnectStreamError {
        code: connect::ConnectCode::Unavailable,
        message:
            "run output exceeded the replay capacity and can no longer be streamed to this client"
                .into(),
        details: Vec::new(),
    })?;
    let mut trace = TraceStreamSink::new(trace, "byok_server");
    let body_stream = async_stream::stream! {
        trace.chunk(&frame);
        trace.finish(end_stream_error(&frame));
        yield Ok::<Bytes, Infallible>(frame);
        let _ = handle
            .command(crate::cursor::conversation::TransportCommand::OutputDetached)
            .await;
    };
    Ok(run_sse_response(Body::from_stream(body_stream)))
}

/// 本地 RunSSE 响应的统一头构造:三个调用点(stream、重放溢出、测试)共用。
fn run_sse_response(body: Body) -> Response<Body> {
    let mut response = Response::new(body);
    *response.status_mut() = StatusCode::OK;
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/event-stream"),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    headers.insert("connect-protocol-version", HeaderValue::from_static("1"));
    response
}

fn local_body_stream(
    receiver: OutputReceiver,
    handle: TransportHandle,
    trace: Option<CursorTraceRecorder>,
) -> impl tokio_stream::Stream<Item = std::result::Result<Bytes, Infallible>> {
    let mut guard = LocalRunGuard::new(handle, receiver);
    async_stream::stream! {
        let mut trace = TraceStreamSink::new(trace, "byok_server");
        while let Some(chunk) = guard.receiver.recv().await {
            let terminal = is_end_stream_frame(&chunk);
            trace.chunk(&chunk);
            if terminal {
                guard.complete();
                trace.finish(end_stream_error(&chunk));
            }
            yield Ok::<Bytes, Infallible>(chunk);
            if terminal {
                return;
            }
        }
        if guard.receiver.overflowed() {
            let frame = slow_subscriber_error_frame();
            trace.chunk(&frame);
            trace.finish(end_stream_error(&frame));
            yield Ok::<Bytes, Infallible>(frame);
            let _ = guard
                .handle
                .command(crate::cursor::conversation::TransportCommand::OutputDetached)
                .await;
        } else {
            trace.finish(None);
        }
        guard.complete();
    }
}

fn slow_subscriber_error_frame() -> Bytes {
    connect::encode_error_end_stream(&connect::ConnectStreamError {
        code: connect::ConnectCode::Unavailable,
        message: "client consumed run output too slowly".into(),
        details: Vec::new(),
    })
    .expect("static slow-subscriber error must encode")
}

fn is_end_stream_frame(frame: &Bytes) -> bool {
    frame
        .first()
        .is_some_and(|flags| flags & END_STREAM_FLAG != 0)
}

fn end_stream_error(frame: &Bytes) -> Option<String> {
    connect::decode_frames(frame)
        .ok()?
        .into_iter()
        .find_map(|(flags, payload)| {
            if flags & END_STREAM_FLAG == 0 {
                return None;
            }
            let value = serde_json::from_slice::<serde_json::Value>(&payload).ok()?;
            let error = value.get("error")?;
            let code = error.get("code").and_then(serde_json::Value::as_str);
            let message = error
                .get("message")
                .and_then(serde_json::Value::as_str)
                .filter(|message| !message.is_empty());
            Some(match (code, message) {
                (Some(code), Some(message)) => format!("{code}: {message}"),
                (Some(code), None) => code.to_string(),
                (None, Some(message)) => message.to_string(),
                (None, None) => error.to_string(),
            })
        })
}

struct LocalRunGuard {
    handle: TransportHandle,
    receiver: OutputReceiver,
    completed: bool,
}

impl LocalRunGuard {
    fn new(handle: TransportHandle, receiver: OutputReceiver) -> Self {
        Self {
            handle,
            receiver,
            completed: false,
        }
    }

    fn complete(&mut self) {
        self.completed = true;
    }
}

impl Drop for LocalRunGuard {
    fn drop(&mut self) {
        self.receiver.close();
        if !self.completed {
            let handle = self.handle.clone();
            tokio::spawn(async move {
                let _ = handle
                    .command(crate::cursor::conversation::TransportCommand::OutputDetached)
                    .await;
            });
        }
    }
}

pub async fn upstream(
    registry: TransportRegistry,
    request_id: String,
    generation: u64,
    response: Response<Body>,
    trace: Option<CursorTraceRecorder>,
) -> Response<Body> {
    let (parts, body) = response.into_parts();
    if let Some(trace) = &trace {
        trace.response_started(parts.status.as_u16());
    }
    let http_error = !parts.status.is_success();
    let stream = async_stream::stream! {
        let _guard = UpstreamRunGuard {
            registry: registry.clone(),
            request_id: request_id.clone(),
            generation,
        };
        let mut trace = TraceStreamSink::new(trace, "cursor_official");
        let mut decoder = connect::FrameDecoder::default();
        let mut error_body = Vec::new();
        let mut terminal_error = None;
        let mut decode_failed = false;
        let mut body = body.into_data_stream();
        while let Some(chunk) = body.next().await {
            match chunk {
                Ok(chunk) => {
                    if http_error {
                        if error_body.len() + chunk.len() <= 1024 * 1024 { error_body.extend_from_slice(&chunk); }
                    } else if !decode_failed {
                        match decoder.push(&chunk) {
                            Ok(frames) => for (flags, payload) in frames {
                                if flags & END_STREAM_FLAG != 0 {
                                    if let Some(error) = crate::cursor::services::official_error::extract_end_stream(&payload) {
                                        registry.fail_upstream_task(&request_id, generation, &error).await;
                                        terminal_error = Some(format!("{}: {}", error.code.as_deref().unwrap_or("upstream_error"), error.message));
                                    }
                                }
                            },
                            Err(error) => { decode_failed = true; terminal_error = Some(error.to_string()); }
                        }
                    }
                    trace.chunk(&chunk);
                    yield Ok::<Bytes, axum::Error>(chunk);
                }
                Err(error) => {
                    trace.finish(Some(error.to_string()));
                    yield Err(error);
                    return;
                }
            }
        }
        if http_error {
            if let Some(error) = crate::cursor::services::official_error::extract_http(&error_body) {
                registry.fail_upstream_task(&request_id, generation, &error).await;
                terminal_error = Some(format!("{}: {}", error.code.as_deref().unwrap_or("upstream_error"), error.message));
            }
        } else if terminal_error.is_none() {
            terminal_error = decoder.finish().err().map(|error| error.to_string());
        }
        trace.finish(terminal_error);
    };
    Response::from_parts(parts, Body::from_stream(stream))
}

enum TraceStreamEvent {
    Chunk(Bytes),
    Finish(Option<String>),
}

struct TraceStreamSink {
    sender: Option<mpsc::UnboundedSender<TraceStreamEvent>>,
}

impl TraceStreamSink {
    fn new(trace: Option<CursorTraceRecorder>, source: &'static str) -> Self {
        let Some(trace) = trace else {
            return Self { sender: None };
        };
        let (sender, mut receiver) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            while let Some(event) = receiver.recv().await {
                match event {
                    TraceStreamEvent::Chunk(chunk) => {
                        trace.response_chunk(source, chunk);
                    }
                    TraceStreamEvent::Finish(error) => {
                        trace.finish(error.as_deref());
                        return;
                    }
                }
            }
            trace.finish(None);
        });
        Self {
            sender: Some(sender),
        }
    }

    fn chunk(&self, chunk: &Bytes) {
        if let Some(sender) = &self.sender {
            let _ = sender.send(TraceStreamEvent::Chunk(chunk.clone()));
        }
    }

    fn finish(&mut self, error: Option<String>) {
        if let Some(sender) = self.sender.take() {
            let _ = sender.send(TraceStreamEvent::Finish(error));
        }
    }
}

impl Drop for TraceStreamSink {
    fn drop(&mut self) {
        if self.sender.is_some() {
            self.finish(Some(
                "response stream dropped before completion".to_string(),
            ));
        }
    }
}

struct UpstreamRunGuard {
    registry: TransportRegistry,
    request_id: String,
    generation: u64,
}

impl Drop for UpstreamRunGuard {
    fn drop(&mut self) {
        self.registry
            .finish_upstream(self.request_id.clone(), self.generation);
    }
}

#[cfg(test)]
mod tests {
    use std::{sync::Arc, time::Duration};

    use super::*;
    use crate::{
        cursor::{
            conversation::TransportCommand, services::observability::CursorTraceService,
            transport::OutputHub,
        },
        store::Store,
    };

    struct EmptyProvider;
    impl crate::provider::Provider for EmptyProvider {
        fn stream(
            &self,
            _: crate::model::ModelInvocation,
            _: tokio_util::sync::CancellationToken,
        ) -> crate::provider::ProviderStream {
            Box::pin(futures_util::stream::empty())
        }
    }

    #[tokio::test]
    async fn official_stream_observation_preserves_split_combined_and_http_error_bodies() {
        let store = Store::connect("sqlite::memory:").await.unwrap();
        let registry = TransportRegistry::new(
            store,
            Arc::new(EmptyProvider),
            crate::cursor::prompting::PromptCompiler::new(
                crate::cursor::prompting::PromptAssets::embedded().unwrap(),
            ),
        );
        let payload = crate::cursor::services::official_error::captured_model_not_found_payload();
        let mut framed = vec![0, 0, 0, 0, 2, 1, 2, END_STREAM_FLAG];
        framed.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        framed.extend_from_slice(&payload);
        let value: serde_json::Value = serde_json::from_slice(&payload).unwrap();
        for (status, wire) in [
            (StatusCode::OK, framed),
            (
                StatusCode::NOT_FOUND,
                serde_json::to_vec(&value["error"]).unwrap(),
            ),
        ] {
            for size in [1, 3, wire.len()] {
                registry.mark_upstream("official-main").await;
                let crate::cursor::TransportRoute::Upstream(generation) =
                    registry.wait_route("official-main").await
                else {
                    panic!()
                };
                let chunks: Vec<_> = wire
                    .chunks(size)
                    .map(|chunk| Ok::<_, Infallible>(Bytes::copy_from_slice(chunk)))
                    .collect();
                let response = Response::builder()
                    .status(status)
                    .body(Body::from_stream(futures_util::stream::iter(chunks)))
                    .unwrap();
                let response = upstream(
                    registry.clone(),
                    "official-main".into(),
                    generation,
                    response,
                    None,
                )
                .await;
                assert_eq!(response.status(), status);
                assert_eq!(
                    axum::body::to_bytes(response.into_body(), 1024 * 1024)
                        .await
                        .unwrap()
                        .as_ref(),
                    wire
                );
            }
        }
    }

    #[tokio::test]
    async fn slow_subscriber_receives_an_error_and_detaches_the_run() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::connect(&format!(
            "sqlite://{}",
            directory.path().join("slow-subscriber.db").display()
        ))
        .await
        .unwrap();
        let trace = CursorTraceService::new(store).recorder("slow-subscriber");
        let (commands, mut command_receiver) = mpsc::channel(4);
        let output = Arc::new(OutputHub::default());
        let handle = TransportHandle::new("slow-subscriber".into(), commands, output, trace);
        let receiver = handle.subscribe().unwrap();

        for _ in 0..2_000 {
            assert!(handle.emit_frame(Bytes::from_static(b"\0")));
        }

        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(1), command_receiver.recv())
                .await
                .unwrap(),
            Some(TransportCommand::OutputDetached)
        ));

        let stream = local_body_stream(receiver, handle, None);
        tokio::pin!(stream);
        let mut terminal_error = None;
        while let Some(frame) = stream.next().await {
            let frame = frame.unwrap();
            if is_end_stream_frame(&frame) {
                terminal_error = end_stream_error(&frame);
            }
        }

        assert_eq!(
            terminal_error.as_deref(),
            Some("unavailable: client consumed run output too slowly")
        );
    }

    /// L-5 回归:同一 chunk 里的双 end-stream(终态后紧随多余终止帧)时,
    /// 接收循环在首个 end-stream 处自然耗尽,不会挂起或重复消费。
    #[tokio::test]
    async fn dual_end_stream_frames_exhaust_the_receiver_naturally() {
        let store = Store::connect("sqlite::memory:").await.unwrap();
        let trace = CursorTraceService::new(store).recorder("dual-end-stream");
        let (commands, _command_receiver) = mpsc::channel(4);
        let output = Arc::new(OutputHub::default());
        let handle = TransportHandle::new("dual-end-stream".into(), commands, output, trace);
        let receiver = handle.subscribe().unwrap();

        let first = connect::encode_end_stream();
        let duplicated = {
            let mut bytes = first.as_ref().to_vec();
            bytes.extend_from_slice(first.as_ref());
            Bytes::from(bytes)
        };
        assert!(handle.emit_frame(duplicated));

        let stream = local_body_stream(receiver, handle, None);
        tokio::pin!(stream);
        let mut terminal_count = 0;
        // 首个 end-stream 终止循环;多余终止帧只留在 OutputHub 历史里,
        // 接收端不会为其阻塞等待。
        while let Some(frame) = tokio::time::timeout(Duration::from_secs(1), stream.next())
            .await
            .expect("receiver must exhaust without waiting for the trailing frame")
        {
            let frame = frame.unwrap();
            if is_end_stream_frame(&frame) {
                terminal_count += 1;
            }
        }
        assert_eq!(terminal_count, 1);
    }

    /// L-4 回归:订阅端断开(Drop)后,后续输出发现无订阅者,触发
    /// OutputDetached,由 runtime 拆除会话。
    #[tokio::test]
    async fn dropping_the_subscriber_detaches_the_unfinished_run() {
        let store = Store::connect("sqlite::memory:").await.unwrap();
        let trace = CursorTraceService::new(store).recorder("detach-on-drop");
        let (commands, mut command_receiver) = mpsc::channel(4);
        let output = Arc::new(OutputHub::default());
        let handle = TransportHandle::new("detach-on-drop".into(), commands, output, trace);

        {
            let receiver = handle.subscribe().unwrap();
            assert!(handle.emit_frame(Bytes::from_static(b"\0")));
            assert!(!receiver.overflowed());
            drop(receiver);
        }
        assert!(!handle.has_subscribers());
        // 后续帧发现最后一个订阅者已消失:发送 OutputDetached。
        assert!(handle.emit_frame(Bytes::from_static(b"\x01")));

        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(1), command_receiver.recv())
                .await
                .unwrap(),
            Some(TransportCommand::OutputDetached)
        ));
    }
}
