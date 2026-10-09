//! Defines commands accepted by a Conversation runtime.

use crate::{cursor::protocol::proto::agent::v1 as pb, Error};

#[derive(Debug)]
pub enum RunFinish {
    TurnCompleted(Box<pb::ConversationStateStructure>),
    Transport(TransportFinish),
}

#[derive(Debug)]
pub enum TransportFinish {
    Success,
    Failed(Error),
    Cancelled,
}

#[derive(Debug)]
pub enum TransportCommand {
    Append {
        seqno: i64,
        message: Box<pb::AgentClientMessage>,
    },
    RunFinished {
        generation: u64,
        finish: RunFinish,
    },
    /// SSE 输出流提前断开;若会话仍有其他订阅者(客户端重连重叠期),
    /// runtime 忽略此次断开继续运行,否则按 Disconnect 拆除。
    OutputDetached,
    /// A detached Task result, owned by the original transport execution scope.
    TaskCompleted {
        owner: u64,
        message: crate::model::CanonicalMessage,
    },
    TaskDelivered {
        owner: u64,
        event_id: String,
        result: crate::run::CommandResult,
    },
    /// The upstream child failed; resolve through the parent's existing execution owner.
    OfficialTaskOwner {
        tool_call_id: String,
        reply: tokio::sync::oneshot::Sender<Option<(u64, u32)>>,
    },
    OfficialTaskFailed {
        owner: u64,
        exec_id: u32,
        message: String,
    },
    Disconnect,
}
