//! Client-to-server wire replies (KV acknowledgements, exec results).

#![allow(dead_code)]

use bytes::Bytes;
use cursor_server::{
    cursor::protocol::{connect, proto::agent::v1 as pb},
    cursor::{TransportCommand, TransportHandle},
};
use prost::Message;

/// A `SetBlobResult { error: None }` KV acknowledgement.
pub fn kv_ack(id: u32) -> pb::AgentClientMessage {
    pb::AgentClientMessage {
        message: Some(pb::agent_client_message::Message::KvClientMessage(
            pb::KvClientMessage {
                id,
                message: Some(pb::kv_client_message::Message::SetBlobResult(
                    pb::SetBlobResult { error: None },
                )),
            },
        )),
    }
}

/// Acknowledge the KV blob write carried by `frame`, if any. End-stream
/// frames are ignored.
pub async fn acknowledge_kv(handle: &TransportHandle, seqno: &mut i64, frame: &[u8]) {
    let (flags, payload) = connect::decode_frames(frame).unwrap().pop().unwrap();
    if flags & connect::END_STREAM_FLAG != 0 {
        return;
    }
    let server = pb::AgentServerMessage::decode(payload).unwrap();
    if let Some(pb::agent_server_message::Message::KvServerMessage(kv)) = server.message {
        handle
            .command(TransportCommand::Append {
                seqno: *seqno,
                message: Box::new(kv_ack(kv.id)),
            })
            .await
            .unwrap();
        *seqno += 1;
    }
}

/// A successful Read tool result.
pub fn read_success(id: u32, path: &str, content: &str) -> pb::AgentClientMessage {
    pb::AgentClientMessage {
        message: Some(pb::agent_client_message::Message::ExecClientMessage(
            pb::ExecClientMessage {
                id,
                message: Some(pb::exec_client_message::Message::ReadResult(
                    pb::ReadResult {
                        result: Some(pb::read_result::Result::Success(pb::ReadSuccess {
                            path: path.into(),
                            total_lines: 1,
                            file_size: 1,
                            output: Some(pb::read_success::Output::Content(content.into())),
                            ..Default::default()
                        })),
                    },
                )),
                ..Default::default()
            },
        )),
    }
}

/// Close the exec stream for `id`.
pub fn stream_close(id: u32) -> pb::AgentClientMessage {
    pb::AgentClientMessage {
        message: Some(pb::agent_client_message::Message::ExecClientControlMessage(
            pb::ExecClientControlMessage {
                message: Some(pb::exec_client_control_message::Message::StreamClose(
                    pb::ExecClientStreamClose { id },
                )),
            },
        )),
    }
}

/// A successful request-context reply.
pub fn request_context_success(id: u32) -> pb::AgentClientMessage {
    pb::AgentClientMessage {
        message: Some(pb::agent_client_message::Message::ExecClientMessage(
            pb::ExecClientMessage {
                id,
                message: Some(pb::exec_client_message::Message::RequestContextResult(
                    pb::RequestContextResult {
                        result: Some(pb::request_context_result::Result::Success(
                            pb::RequestContextSuccess {
                                request_context: Some(pb::RequestContext::default()),
                                ..Default::default()
                            },
                        )),
                    },
                )),
                ..Default::default()
            },
        )),
    }
}

/// A successful subagent result for `agent_id`.
pub fn subagent_result_success(id: u32, agent_id: &str) -> pb::AgentClientMessage {
    pb::AgentClientMessage {
        message: Some(pb::agent_client_message::Message::ExecClientMessage(
            pb::ExecClientMessage {
                id,
                message: Some(pb::exec_client_message::Message::SubagentResult(
                    pb::SubagentResult {
                        result: Some(pb::subagent_result::Result::Success(pb::SubagentSuccess {
                            agent_id: agent_id.into(),
                            ..Default::default()
                        })),
                    },
                )),
                ..Default::default()
            },
        )),
    }
}

/// A completed `AWAIT` result for `agent_id`.
pub fn subagent_await_complete(id: u32, agent_id: &str) -> pb::AgentClientMessage {
    pb::AgentClientMessage {
        message: Some(pb::agent_client_message::Message::ExecClientMessage(
            pb::ExecClientMessage {
                id,
                message: Some(pb::exec_client_message::Message::SubagentAwaitResult(
                    pb::SubagentAwaitResult {
                        result: Some(pb::subagent_await_result::Result::Complete(
                            pb::SubagentAwaitComplete {
                                agent_id: agent_id.into(),
                                final_message: Some("child result".into()),
                                ..Default::default()
                            },
                        )),
                    },
                )),
                ..Default::default()
            },
        )),
    }
}

/// An errored subagent result.
pub fn subagent_result_error(id: u32, error: &str) -> pb::AgentClientMessage {
    pb::AgentClientMessage {
        message: Some(pb::agent_client_message::Message::ExecClientMessage(
            pb::ExecClientMessage {
                id,
                message: Some(pb::exec_client_message::Message::SubagentResult(
                    pb::SubagentResult {
                        result: Some(pb::subagent_result::Result::Error(pb::SubagentError {
                            agent_id: None,
                            error: error.into(),
                        })),
                    },
                )),
                ..Default::default()
            },
        )),
    }
}
