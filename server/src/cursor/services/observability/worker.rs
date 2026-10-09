use std::{
    collections::{BTreeMap, HashMap},
    sync::{
        atomic::{AtomicU8, Ordering},
        Arc,
    },
    time::Duration,
};

use parking_lot::Mutex;
use tokio::sync::mpsc;

use crate::store::{BufferedCursorTraceChunk, Store};

use super::event::{TraceEvent, TRACE_ACTIVE, TRACE_DISABLED, TRACE_UNKNOWN};
use super::DecodedMessage;

const MAX_BUFFERED_CHUNKS: usize = 32;
const MAX_BUFFERED_BYTES: usize = 256 * 1024;
const FLUSH_INTERVAL: Duration = Duration::from_millis(50);

#[derive(Clone, Copy, PartialEq, Eq)]
enum TraceState {
    /// The trace row exists, so events can be persisted immediately.
    Active,
    /// Detailed logging is enabled, but `Begin` has not created the trace row yet.
    Pending,
    Disabled,
}

#[derive(Default)]
struct ResponseBuffer {
    chunks: Vec<BufferedCursorTraceChunk>,
    bytes: usize,
}

struct BufferedRequest {
    artifact_type: String,
    data: bytes::Bytes,
    decoded: Option<DecodedMessage>,
    metadata: serde_json::Value,
}

#[derive(Default)]
struct RequestOrder {
    /// The first observed append anchors traces that begin after seqno 0.
    next: Option<i64>,
    pending: BTreeMap<i64, Vec<BufferedRequest>>,
}

struct TrackedRequest {
    state: TraceState,
    /// RunSSE can subscribe before the first Bidi `RunRequest` reaches `Begin`.
    /// Preserve those events and replay them in arrival order once the row exists.
    pending: Vec<TraceEvent>,
}

pub(super) async fn run(
    store: Store,
    mut receiver: mpsc::UnboundedReceiver<TraceEvent>,
    activations: Arc<Mutex<HashMap<Arc<str>, Arc<AtomicU8>>>>,
) {
    let mut states = HashMap::<String, TrackedRequest>::new();
    let mut buffers = HashMap::<String, ResponseBuffer>::new();
    let mut request_orders = HashMap::<String, RequestOrder>::new();
    let mut interval = tokio::time::interval(FLUSH_INTERVAL);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            event = receiver.recv() => {
                let Some(event) = event else {
                    flush_all(&store, &mut buffers).await;
                    return;
                };
                if let Some(request_id) = process(
                    &store,
                    &mut states,
                    &mut buffers,
                    &mut request_orders,
                    event,
                ).await {
                    activations.lock().remove(request_id.as_str());
                }
            }
            _ = interval.tick() => flush_all(&store, &mut buffers).await,
        }
    }
}

async fn process(
    store: &Store,
    states: &mut HashMap<String, TrackedRequest>,
    buffers: &mut HashMap<String, ResponseBuffer>,
    request_orders: &mut HashMap<String, RequestOrder>,
    event: TraceEvent,
) -> Option<String> {
    let request_id = event.request_id().to_owned();
    match event {
        TraceEvent::Begin {
            request_id,
            activation,
            conversation_id,
            route,
            model_id,
        } => {
            let state = match store
                .start_cursor_trace_if_detailed(
                    &request_id,
                    conversation_id.as_deref(),
                    &route,
                    model_id.as_deref(),
                )
                .await
            {
                Ok(true) => TraceState::Active,
                Ok(false) => TraceState::Disabled,
                Err(error) => {
                    tracing::warn!(%request_id, %error, "failed to start Cursor trace");
                    TraceState::Disabled
                }
            };
            activation.store(activation_value(state), Ordering::Release);
            let pending = states
                .remove(&request_id)
                .map(|tracked| tracked.pending)
                .unwrap_or_default();
            states.insert(
                request_id.clone(),
                TrackedRequest {
                    state,
                    pending: Vec::new(),
                },
            );
            let pending_finished = pending
                .iter()
                .any(|event| matches!(event, TraceEvent::Finish { .. }));
            if state == TraceState::Active {
                for event in pending {
                    if process_active(store, buffers, request_orders, &request_id, event).await {
                        clear_request(states, buffers, request_orders, &request_id);
                        return Some(request_id);
                    }
                }
            } else if pending_finished {
                clear_request(states, buffers, request_orders, &request_id);
                return Some(request_id);
            }
            None
        }
        TraceEvent::Resume {
            request_id,
            activation,
        } => {
            match ensure_state(store, states, &request_id).await {
                // Keep the shared activation undecided until `Begin` creates the row.
                TraceState::Pending => {}
                state => activation.store(activation_value(state), Ordering::Release),
            }
            None
        }
        event => match ensure_state(store, states, &request_id).await {
            TraceState::Active => {
                if process_active(store, buffers, request_orders, &request_id, event).await {
                    clear_request(states, buffers, request_orders, &request_id);
                    Some(request_id)
                } else {
                    None
                }
            }
            TraceState::Pending => {
                states
                    .get_mut(&request_id)
                    .expect("pending trace state exists")
                    .pending
                    .push(event);
                None
            }
            TraceState::Disabled => {
                if matches!(event, TraceEvent::Finish { .. }) {
                    clear_request(states, buffers, request_orders, &request_id);
                    Some(request_id)
                } else {
                    None
                }
            }
        },
    }
}

async fn process_active(
    store: &Store,
    buffers: &mut HashMap<String, ResponseBuffer>,
    request_orders: &mut HashMap<String, RequestOrder>,
    request_id: &str,
    event: TraceEvent,
) -> bool {
    let finishes_trace = matches!(&event, TraceEvent::Finish { .. });
    // Response chunks are batched, but a later non-chunk event is an ordering
    // boundary. Flush first so artifact `seq` remains a faithful event timeline.
    if !matches!(&event, TraceEvent::ResponseChunk { .. }) {
        flush_one(store, buffers, request_id).await;
    }
    let result = match event {
        TraceEvent::Request {
            artifact_type,
            data,
            decoded,
            metadata,
            ..
        } => {
            append_request(
                store,
                request_orders,
                request_id,
                artifact_type,
                data,
                decoded,
                metadata,
            )
            .await
        }
        TraceEvent::Artifact {
            artifact_type,
            source,
            data,
            metadata,
            ..
        } => {
            store
                .append_cursor_trace_artifact(request_id, &artifact_type, &source, &data, &metadata)
                .await
        }
        TraceEvent::LinkedBlob {
            artifact_type,
            source,
            blob_id,
            metadata,
            ..
        } => {
            store
                .link_cursor_trace_artifact(
                    request_id,
                    &artifact_type,
                    &source,
                    &blob_id,
                    &metadata,
                )
                .await
        }
        TraceEvent::ResponseStarted { status, .. } => {
            store.start_cursor_trace_response(request_id, status).await
        }
        TraceEvent::ResponseChunk { source, data, .. } => {
            let buffer = buffers.entry(request_id.to_owned()).or_default();
            buffer.bytes += data.len();
            buffer
                .chunks
                .push(BufferedCursorTraceChunk::new(&source, &data));
            if buffer.chunks.len() >= MAX_BUFFERED_CHUNKS || buffer.bytes >= MAX_BUFFERED_BYTES {
                flush_one(store, buffers, request_id).await;
            }
            return false;
        }
        TraceEvent::Finish { error, .. } => {
            flush_request_order(store, request_orders, request_id).await;
            flush_one(store, buffers, request_id).await;
            store
                .finish_cursor_trace(request_id, error.as_deref())
                .await
        }
        TraceEvent::Begin { .. } | TraceEvent::Resume { .. } => unreachable!(),
    };
    if let Err(error) = result {
        tracing::warn!(%request_id, %error, "failed to record Cursor trace event");
    }
    finishes_trace
}

fn activation_value(state: TraceState) -> u8 {
    match state {
        TraceState::Active => TRACE_ACTIVE,
        TraceState::Pending => TRACE_UNKNOWN,
        TraceState::Disabled => TRACE_DISABLED,
    }
}

fn clear_request(
    states: &mut HashMap<String, TrackedRequest>,
    buffers: &mut HashMap<String, ResponseBuffer>,
    request_orders: &mut HashMap<String, RequestOrder>,
    request_id: &str,
) {
    states.remove(request_id);
    buffers.remove(request_id);
    request_orders.remove(request_id);
}

async fn append_request(
    store: &Store,
    request_orders: &mut HashMap<String, RequestOrder>,
    request_id: &str,
    artifact_type: String,
    data: bytes::Bytes,
    decoded: Option<DecodedMessage>,
    metadata: serde_json::Value,
) -> crate::Result<()> {
    let append_seqno = metadata
        .get("append_seqno")
        .and_then(serde_json::Value::as_i64);
    let Some(append_seqno) = append_seqno.filter(|_| artifact_type == "bidi_request") else {
        return append_client_request(
            store,
            request_id,
            &artifact_type,
            &data,
            decoded.as_ref(),
            &metadata,
        )
        .await;
    };

    let request = BufferedRequest {
        artifact_type,
        data,
        decoded,
        metadata,
    };
    let order = request_orders.entry(request_id.to_owned()).or_default();
    let next = *order.next.get_or_insert(append_seqno);
    if append_seqno < next {
        return append_client_request(
            store,
            request_id,
            &request.artifact_type,
            &request.data,
            request.decoded.as_ref(),
            &request.metadata,
        )
        .await;
    }
    order.pending.entry(append_seqno).or_default().push(request);
    loop {
        let next = order.next.expect("request order is anchored");
        let Some(requests) = order.pending.remove(&next) else {
            break;
        };
        for request in requests {
            append_client_request(
                store,
                request_id,
                &request.artifact_type,
                &request.data,
                request.decoded.as_ref(),
                &request.metadata,
            )
            .await?;
        }
        order.next = Some(next.saturating_add(1));
    }
    Ok(())
}

/// Persist one raw append and its readable protobuf JSON view next to each other.
async fn append_client_request(
    store: &Store,
    request_id: &str,
    artifact_type: &str,
    data: &[u8],
    decoded: Option<&DecodedMessage>,
    metadata: &serde_json::Value,
) -> crate::Result<()> {
    store
        .append_cursor_trace_request(request_id, artifact_type, "cursor_client", data, metadata)
        .await?;
    if let Some(decoded) = decoded {
        store
            .append_cursor_trace_artifact(
                request_id,
                "client_message",
                "cursor_client",
                &decoded.data,
                &decoded.metadata,
            )
            .await?;
    }
    Ok(())
}

async fn flush_request_order(
    store: &Store,
    request_orders: &mut HashMap<String, RequestOrder>,
    request_id: &str,
) {
    let Some(order) = request_orders.remove(request_id) else {
        return;
    };
    for requests in order.pending.into_values() {
        for request in requests {
            if let Err(error) = append_client_request(
                store,
                request_id,
                &request.artifact_type,
                &request.data,
                request.decoded.as_ref(),
                &request.metadata,
            )
            .await
            {
                tracing::warn!(%request_id, %error, "failed to flush ordered Cursor request trace");
            }
        }
    }
}

async fn ensure_state(
    store: &Store,
    states: &mut HashMap<String, TrackedRequest>,
    request_id: &str,
) -> TraceState {
    if let Some(tracked) = states.get(request_id) {
        return tracked.state;
    }
    let state = match store.cursor_trace_status(request_id).await {
        Ok(Some(status)) if status == "running" => TraceState::Active,
        Ok(Some(_)) => TraceState::Disabled,
        Ok(None) => match store.detailed_logging().await {
            Ok(true) => TraceState::Pending,
            Ok(false) => TraceState::Disabled,
            Err(error) => {
                tracing::warn!(%request_id, %error, "failed to read detailed logging state");
                TraceState::Disabled
            }
        },
        Err(error) => {
            tracing::warn!(%request_id, %error, "failed to resume Cursor trace");
            TraceState::Disabled
        }
    };
    states.insert(
        request_id.to_owned(),
        TrackedRequest {
            state,
            pending: Vec::new(),
        },
    );
    state
}

async fn flush_one(store: &Store, buffers: &mut HashMap<String, ResponseBuffer>, request_id: &str) {
    let Some(mut buffer) = buffers.remove(request_id) else {
        return;
    };
    if let Err(error) = store
        .add_cursor_trace_response_chunks(request_id, &buffer.chunks)
        .await
    {
        tracing::warn!(%request_id, %error, "failed to flush Cursor response chunks");
        buffer.bytes = buffer.chunks.iter().map(|chunk| chunk.data.len()).sum();
        buffers.insert(request_id.to_owned(), buffer);
    }
}

async fn flush_all(store: &Store, buffers: &mut HashMap<String, ResponseBuffer>) {
    let request_ids = buffers.keys().cloned().collect::<Vec<_>>();
    for request_id in request_ids {
        flush_one(store, buffers, &request_id).await;
    }
}
