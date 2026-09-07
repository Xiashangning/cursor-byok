//! Drives a subscribed transport to EndStream while collecting server output.

#![allow(dead_code)]

use std::collections::HashMap;

use bytes::Bytes;
use cursor_server::{
    cursor::protocol::{connect, proto::agent::v1 as pb},
    cursor::{TransportCommand, TransportHandle},
};
use prost::Message;

/// Everything a run streamed before its terminal EndStream frame.
#[derive(Default)]
pub struct PumpOutput {
    /// Blob writes captured from `SetBlobArgs`, keyed by blob id.
    pub blobs: HashMap<Vec<u8>, Vec<u8>>,
    /// Raw KV server messages, in arrival order.
    pub kvs: Vec<pb::KvServerMessage>,
    /// `ConversationCheckpointUpdate` states, in arrival order.
    pub checkpoints: Vec<pb::ConversationStateStructure>,
    /// `InteractionUpdate` events, in arrival order.
    pub interactions: Vec<pb::InteractionUpdate>,
    /// Exec requests sent to the client, in arrival order.
    pub execs: Vec<pb::ExecServerMessage>,
    /// Decoded JSON body of the terminal EndStream frame.
    pub terminal: serde_json::Value,
}

/// Consume `output` until the run's EndStream frame, acknowledging every KV
/// blob write. Exec requests are passed to `on_exec`, whose returned messages
/// are appended in order. Replies to other frames are handled internally.
pub async fn drive(
    handle: &TransportHandle,
    output: &mut tokio::sync::mpsc::UnboundedReceiver<Bytes>,
    seqno: &mut i64,
    mut on_exec: impl FnMut(&pb::ExecServerMessage) -> Vec<pb::AgentClientMessage>,
) -> PumpOutput {
    let mut out = PumpOutput::default();
    loop {
        let frame = tokio::time::timeout(std::time::Duration::from_secs(5), output.recv())
            .await
            .unwrap()
            .expect("RunSSE closed before EndStream");
        let (flags, payload) = connect::decode_frames(&frame).unwrap().pop().unwrap();
        if flags & connect::END_STREAM_FLAG != 0 {
            out.terminal = serde_json::from_slice(&payload).unwrap();
            return out;
        }
        let server = pb::AgentServerMessage::decode(payload).unwrap();
        match server.message {
            Some(pb::agent_server_message::Message::KvServerMessage(kv)) => {
                if let Some(pb::kv_server_message::Message::SetBlobArgs(set)) = &kv.message {
                    out.blobs.insert(set.blob_id.clone(), set.blob_data.clone());
                }
                let kv_id = kv.id;
                out.kvs.push(kv);
                handle
                    .command(TransportCommand::Append {
                        seqno: *seqno,
                        message: Box::new(super::wire::kv_ack(kv_id)),
                    })
                    .await
                    .unwrap();
                *seqno += 1;
            }
            Some(pb::agent_server_message::Message::ExecServerMessage(exec)) => {
                let replies = on_exec(&exec);
                out.execs.push(exec);
                for reply in replies {
                    handle
                        .command(TransportCommand::Append {
                            seqno: *seqno,
                            message: Box::new(reply),
                        })
                        .await
                        .unwrap();
                    *seqno += 1;
                }
            }
            Some(pb::agent_server_message::Message::ConversationCheckpointUpdate(state)) => {
                out.checkpoints.push(state)
            }
            Some(pb::agent_server_message::Message::InteractionUpdate(update)) => {
                out.interactions.push(update)
            }
            _ => {}
        }
    }
}

/// Concatenate every streamed `TextDelta` in `output`.
pub fn text_of(output: &PumpOutput) -> String {
    output
        .interactions
        .iter()
        .filter_map(|update| match &update.message {
            Some(pb::interaction_update::Message::TextDelta(delta)) => Some(delta.text.as_str()),
            _ => None,
        })
        .collect()
}
