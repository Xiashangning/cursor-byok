//! Hydrates request context blobs supplied by Cursor.
use std::{collections::HashMap, sync::Arc, time::Duration};

use parking_lot::Mutex;
use prost::Message;
use tokio::sync::oneshot;

use crate::{
    cursor::{protocol::proto::agent::v1 as pb, transport::TransportHandle},
    store::{BlobId, Store},
    Error, Result,
};

type ContextSender = oneshot::Sender<Result<pb::RequestContext>>;

#[derive(Clone)]
pub(crate) struct RequestContextSynchronizer {
    handle: TransportHandle,
    store: Store,
    pending: Arc<Mutex<HashMap<u32, ContextSender>>>,
    runtime: crate::cursor::tools::runtime::CursorToolRuntime,
}

struct PendingContext {
    id: u32,
    pending: Arc<Mutex<HashMap<u32, ContextSender>>>,
}

impl Drop for PendingContext {
    fn drop(&mut self) {
        self.pending.lock().remove(&self.id);
    }
}

impl RequestContextSynchronizer {
    pub(crate) fn new(
        handle: TransportHandle,
        store: Store,
        runtime: crate::cursor::tools::runtime::CursorToolRuntime,
    ) -> Self {
        Self {
            handle,
            store,
            pending: Arc::new(Mutex::new(HashMap::new())),
            runtime,
        }
    }

    pub(crate) async fn refresh_if_missing(
        &self,
        references: &pb::RequestContextPartReferences,
        conversation_id: &str,
    ) -> Result<Option<pb::RequestContext>> {
        if !self.has_missing_part(references).await? {
            return Ok(None);
        }
        let context = self.load(conversation_id).await?;
        self.cache_parts(&context).await?;
        Ok(Some(context))
    }

    pub(crate) async fn get(&self, id: &BlobId) -> Result<Option<Vec<u8>>> {
        self.store.get_blob(id).await
    }

    pub(crate) async fn load(&self, conversation_id: &str) -> Result<pb::RequestContext> {
        let (sender, receiver) = oneshot::channel();
        let id = self.runtime.next_id()?;
        self.pending.lock().insert(id, sender);
        let _pending = PendingContext {
            id,
            pending: self.pending.clone(),
        };

        tracing::info!(
            request_id = self.handle.request_id(),
            conversation_id,
            "requesting uncached Cursor context"
        );

        self.handle.emit(&pb::AgentServerMessage {
            ttft_breakdown: None,
            message: Some(pb::agent_server_message::Message::ExecServerMessage(
                pb::ExecServerMessage {
                    id,
                    message: Some(pb::exec_server_message::Message::RequestContextArgs(
                        pb::RequestContextArgs {
                            notes_session_id: Some(conversation_id.into()),
                            ..Default::default()
                        },
                    )),
                    ..Default::default()
                },
            )),
        })?;

        let cancellation = self.handle.disconnect_token();
        tokio::select! {
            result = receiver => result.map_err(|_| Error::Protocol("request context response channel closed".into()))?,
            _ = cancellation.cancelled() => Err(Error::Cancelled),
            _ = tokio::time::sleep(Duration::from_secs(60)) => Err(Error::Protocol("request context timed out".into())),
        }
    }

    pub(crate) async fn handle_client(&self, message: &pb::ExecClientMessage) -> bool {
        let Some(pb::exec_client_message::Message::RequestContextResult(result)) =
            message.message.as_ref()
        else {
            return false;
        };
        let Some(sender) = self.pending.lock().remove(&message.id) else {
            tracing::warn!(
                request_id = self.handle.request_id(),
                "unexpected Cursor request context result"
            );
            return true;
        };
        use pb::request_context_result::Result as ContextResult;
        let result = match result.result.as_ref() {
            Some(ContextResult::Success(success)) => success
                .request_context
                .clone()
                .ok_or_else(|| Error::Protocol("Cursor returned empty request context".into())),
            Some(ContextResult::Error(error)) => Err(Error::Protocol(format!(
                "Cursor request context failed: {}",
                error.error
            ))),
            Some(ContextResult::Rejected(rejected)) => Err(Error::Protocol(format!(
                "Cursor rejected request context: {}",
                rejected.reason
            ))),
            None => Err(Error::Protocol(
                "Cursor returned no request context result".into(),
            )),
        };
        let _ = sender.send(result);
        true
    }

    pub(crate) async fn handle_stream_close(&self, id: u32) -> bool {
        self.pending.lock().contains_key(&id)
    }

    pub(crate) async fn handle_throw(&self, id: u32, message: String) -> bool {
        let sender = self.pending.lock().remove(&id);
        let Some(sender) = sender else { return false };
        let _ = sender.send(Err(Error::Protocol(message)));
        true
    }

    async fn has_missing_part(&self, parts: &pb::RequestContextPartReferences) -> Result<bool> {
        for raw_id in [
            parts.rules_blob_id.as_slice(),
            parts.skills_blob_id.as_slice(),
            parts.subagents_blob_id.as_slice(),
            parts.mcps_blob_id.as_slice(),
        ] {
            if raw_id.is_empty() {
                continue;
            }
            let id = BlobId::from_bytes(raw_id)?;
            if self.store.get_blob(&id).await?.is_none() {
                return Ok(true);
            }
        }
        Ok(false)
    }

    async fn cache_parts(&self, context: &pb::RequestContext) -> Result<()> {
        self.cache_part(&pb::RequestContextRulesPart {
            rules: context.rules.clone(),
            non_file_rules: context.non_file_rules.clone(),
            cloud_rule: context.cloud_rule.clone(),
        })
        .await?;
        self.cache_part(&pb::RequestContextSkillsPart {
            agent_skills: context.agent_skills.clone(),
            skill_options: context.skill_options.clone(),
        })
        .await?;
        self.cache_part(&pb::RequestContextSubagentsPart {
            custom_subagents: context.custom_subagents.clone(),
        })
        .await?;
        self.cache_part(&pb::RequestContextMcpsPart {
            tools: context.tools.clone(),
            mcp_instructions: context.mcp_instructions.clone(),
            mcp_file_system_options: context.mcp_file_system_options.clone(),
            mcp_meta_tool_options: context.mcp_meta_tool_options.clone(),
        })
        .await
    }

    async fn cache_part<T: Message>(&self, part: &T) -> Result<()> {
        let data = part.encode_to_vec();
        self.store.put_blob(&data, &[]).await?;
        Ok(())
    }
}
