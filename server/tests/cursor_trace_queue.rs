//! Verifies that Cursor trace persistence is ordered and detached from producers.

mod support;

use std::time::{Duration, Instant};

use bytes::Bytes;
use cursor_server::{
    cursor::services::observability::{CursorTraceService, DecodedMessage},
    store::Store,
};
use sqlx::{Connection, SqliteConnection};
use support::temp_store;

#[tokio::test]
async fn trace_producers_do_not_wait_for_sqlite_and_artifacts_stay_ordered() {
    let directory = tempfile::tempdir().unwrap();
    let url = format!("sqlite://{}", directory.path().join("test.db").display());
    let store = Store::connect(&url).await.unwrap();
    store.set_detailed_logging(true).await.unwrap();
    let traces = CursorTraceService::new(store.clone());
    let recorder = traces.recorder("trace-queue-order");
    recorder.begin(Some("conversation-1"), "local_byok", Some("model-1"));

    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if store
                .cursor_trace("trace-queue-order")
                .await
                .unwrap()
                .is_some()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();

    let mut write_lock = SqliteConnection::connect(&url).await.unwrap();
    sqlx::query("BEGIN IMMEDIATE")
        .execute(&mut write_lock)
        .await
        .unwrap();
    recorder.bidi_append(
        Bytes::from_static(b"request-5"),
        serde_json::json!({
            "append_seqno": 5,
            "accepted": true,
            "route_outcome": "local"
        }),
        None,
    );
    tokio::time::sleep(Duration::from_millis(25)).await;

    let started = Instant::now();
    let mut seqnos = (6..69).collect::<Vec<_>>();
    for pair in seqnos.chunks_mut(2) {
        pair.reverse();
    }
    for seqno in seqnos {
        recorder.bidi_append(
            Bytes::from(format!("request-{seqno}")),
            serde_json::json!({
                "append_seqno": seqno,
                "accepted": seqno != 17,
                "route_outcome": if seqno == 17 { "invalid_parent" } else { "local" }
            }),
            None,
        );
    }
    assert!(started.elapsed() < Duration::from_millis(100));
    sqlx::query("ROLLBACK")
        .execute(&mut write_lock)
        .await
        .unwrap();

    let artifacts = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let artifacts = store
                .cursor_trace_artifacts("trace-queue-order")
                .await
                .unwrap();
            if artifacts.len() == 64 {
                break artifacts;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();

    for (index, artifact) in artifacts.iter().enumerate() {
        assert_eq!(artifact.seq, index as i64);
        assert_eq!(artifact.metadata["append_seqno"], index as i64 + 5);
    }
    recorder.finish(None);
    let trace = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let trace = store
                .cursor_trace("trace-queue-order")
                .await
                .unwrap()
                .unwrap();
            if trace.status == "completed" {
                break trace;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(trace.status, "completed");
    assert_eq!(
        trace.request_bytes,
        (5..69)
            .map(|seqno| format!("request-{seqno}").len() as i64)
            .sum::<i64>()
    );
}

#[tokio::test]
async fn events_for_disabled_detailed_logging_are_discarded_off_path() {
    let (_directory, store) = temp_store().await;
    let traces = CursorTraceService::new(store.clone());
    let subscriber = traces.recorder("trace-disabled");
    subscriber.resume();
    tokio::time::timeout(Duration::from_secs(2), async {
        while subscriber.is_enabled() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();

    let recorder = traces.recorder("trace-disabled");
    assert!(
        !recorder.is_enabled(),
        "recorders for one request must share the disabled activation"
    );
    recorder.begin(None, "local_byok", Some("model-1"));
    recorder.bidi_append(
        Bytes::from_static(b"body"),
        serde_json::json!({"append_seqno": 0}),
        None,
    );

    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(store
        .cursor_trace("trace-disabled")
        .await
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn new_requests_read_the_current_detailed_logging_setting() {
    let (_directory, store) = temp_store().await;
    let traces = CursorTraceService::new(store.clone());

    for (index, enabled) in [true, false, true].into_iter().enumerate() {
        store.set_detailed_logging(enabled).await.unwrap();
        let request_id = format!("trace-setting-{index}");
        let recorder = traces.recorder(&request_id);
        recorder.begin(None, "local_byok", Some("model-1"));
        recorder.response_started(200);
        recorder.bidi_append(
            Bytes::from_static(b"request"),
            serde_json::json!({"append_seqno": 0}),
            None,
        );
        recorder.finish(None);

        let trace = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let trace = store.cursor_trace(&request_id).await.unwrap();
                if enabled {
                    if trace.as_ref().is_some_and(|trace| trace.status == "completed") {
                        break trace;
                    }
                } else if !recorder.is_enabled() {
                    break trace;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        let artifacts = store.cursor_trace_artifacts(&request_id).await.unwrap();
        if enabled {
            let trace = trace.expect("detailed request is recorded");
            assert_eq!(trace.http_status, Some(200));
            assert_eq!(trace.request_bytes, 7);
            assert_eq!(artifacts.len(), 1);
            assert_eq!(artifacts[0].data, b"request");
        } else {
            assert!(trace.is_none());
            assert!(artifacts.is_empty());
        }
    }
}

#[tokio::test]
async fn events_before_begin_are_replayed_in_order_and_finish_the_trace() {
    let (_directory, store) = temp_store().await;
    store.set_detailed_logging(true).await.unwrap();
    let blob_id = store.put_blob(b"blob-body", &[]).await.unwrap();
    let traces = CursorTraceService::new(store.clone());

    // RunSSE and the transport can exist before the first Bidi RunRequest creates
    // the trace row. Every event must remain pending rather than being discarded.
    let subscriber = traces.recorder("trace-late-begin");
    subscriber.resume();
    subscriber.response_started(200);
    subscriber.response_chunk("byok_server", Bytes::from_static(b"sse-frame"));
    subscriber.artifact(
        "checkpoint",
        "byok_server",
        br#"{"turns":[]}"#,
        serde_json::json!({"emit_status": "sent"}),
    );
    subscriber.linked_blob(
        "blob_set",
        "byok_server",
        &blob_id,
        serde_json::json!({"status": "acknowledged"}),
    );

    let request = traces.recorder("trace-late-begin");
    request.bidi_append(
        Bytes::from_static(b"bidi-frame"),
        serde_json::json!({
            "append_seqno": 0,
            "accepted": true,
            "route_outcome": "local"
        }),
        Some(DecodedMessage {
            data: Bytes::from_static(br#"{"runRequest":{"conversationId":"conversation-1"}}"#),
            metadata: serde_json::json!({"message_type": "run_request"}),
        }),
    );
    subscriber.finish(None);

    traces.recorder("trace-late-begin").begin(
        Some("conversation-1"),
        "local_byok",
        Some("model-1"),
    );

    let trace = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(trace) = store.cursor_trace("trace-late-begin").await.unwrap() {
                if trace.status == "completed" {
                    break trace;
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(trace.conversation_id.as_deref(), Some("conversation-1"));
    assert_eq!(trace.http_status, Some(200));
    assert_eq!(trace.response_bytes, 9);

    let artifacts = store
        .cursor_trace_artifacts("trace-late-begin")
        .await
        .unwrap();
    assert_eq!(
        artifacts
            .iter()
            .map(|artifact| artifact.artifact_type.as_str())
            .collect::<Vec<_>>(),
        [
            "run_sse_chunk",
            "checkpoint",
            "blob_set",
            "bidi_request",
            "client_message",
        ]
    );
    assert_eq!(artifacts[0].data, b"sse-frame");
    assert_eq!(artifacts[3].metadata["append_seqno"], 0);
    assert_eq!(artifacts[4].metadata["message_type"], "run_request");

    let finished_at_ms = trace.finished_at_ms;
    let resumed = traces.recorder("trace-late-begin");
    resumed.resume();
    tokio::time::timeout(Duration::from_secs(2), async {
        while resumed.is_enabled() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    resumed.response_started(201);
    resumed.response_chunk("byok_server", Bytes::from_static(b"late-frame"));
    resumed.artifact(
        "server_message",
        "byok_server",
        b"late-message",
        serde_json::json!({}),
    );
    resumed.finish(Some("late finish"));
    store
        .start_cursor_trace_response("trace-late-begin", 202)
        .await
        .unwrap();
    store
        .finish_cursor_trace("trace-late-begin", Some("second finish"))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(25)).await;

    let terminal = store
        .cursor_trace("trace-late-begin")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(terminal.status, "completed");
    assert_eq!(terminal.http_status, Some(200));
    assert_eq!(terminal.finished_at_ms, finished_at_ms);
    assert_eq!(terminal.error_message, None);
    assert_eq!(
        store
            .cursor_trace_artifacts("trace-late-begin")
            .await
            .unwrap()
            .len(),
        artifacts.len()
    );
}
