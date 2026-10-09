//! Bounded waits for asynchronous test state.

#![allow(dead_code)]

use bytes::Bytes;
use cursor_server::cursor::{TransportCommand, TransportHandle};
use prost::Message;

use super::{fake_provider::FakeProvider, wire::acknowledge_kv};
use cursor_server::cursor::protocol::connect;

/// Poll until the provider has recorded `count` requests, consuming and
/// acknowledging frames while waiting. Panics if the run ends first or the
/// 5s deadline expires, instead of hanging.
pub async fn wait_for_provider_requests(
    provider: &FakeProvider,
    handle: &TransportHandle,
    output: &mut tokio::sync::mpsc::Receiver<Bytes>,
    seqno: &mut i64,
    count: usize,
) {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    while provider.request_count() < count {
        assert!(
            tokio::time::Instant::now() < deadline,
            "provider did not reach {count} requests"
        );
        if let Ok(Some(frame)) =
            tokio::time::timeout(std::time::Duration::from_millis(20), output.recv()).await
        {
            let (flags, payload) = connect::decode_frames(&frame).unwrap().pop().unwrap();
            if flags & connect::END_STREAM_FLAG != 0 {
                panic!(
                    "run ended before provider call {count}: {}",
                    String::from_utf8_lossy(&payload)
                );
            }
            acknowledge_kv(handle, seqno, &frame).await;
        }
    }
}
