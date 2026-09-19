//! Persists background subagent completions already consumed synchronously by the parent.

use crate::{model::ConversationId, Result};

use super::{now_ms, Store};

impl Store {
    pub(crate) async fn record_consumed_subagent_completion(
        &self,
        conversation_id: &ConversationId,
        subagent_id: &str,
        parent_tool_call_id: &str,
    ) -> Result<()> {
        let _write = self.writes.lock().await;
        sqlx::query(
            "INSERT OR IGNORE INTO consumed_background_completions
             (conversation_id, subagent_id, parent_tool_call_id, created_at_ms)
             VALUES (?, ?, ?, ?)",
        )
        .bind(conversation_id.as_str())
        .bind(subagent_id)
        .bind(parent_tool_call_id)
        .bind(now_ms())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub(crate) async fn consumed_subagent_completion(
        &self,
        conversation_id: &ConversationId,
        subagent_id: &str,
        parent_tool_call_id: &str,
    ) -> Result<bool> {
        Ok(sqlx::query_scalar::<_, i64>(
            "SELECT EXISTS(
                 SELECT 1 FROM consumed_background_completions
                 WHERE conversation_id = ?
                   AND subagent_id = ?
                   AND parent_tool_call_id = ?
             )",
        )
        .bind(conversation_id.as_str())
        .bind(subagent_id)
        .bind(parent_tool_call_id)
        .fetch_one(&self.pool)
        .await?
            != 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn consumed_completion_round_trips() {
        let directory = tempfile::tempdir().unwrap();
        let url = format!("sqlite://{}", directory.path().join("test.db").display());
        let store = Store::connect(&url).await.unwrap();
        let conversation_id = ConversationId::new("conversation-1");
        store.ensure_conversation(&conversation_id).await.unwrap();

        assert!(!store
            .consumed_subagent_completion(&conversation_id, "agent-1", "task-call-1")
            .await
            .unwrap());
        store
            .record_consumed_subagent_completion(&conversation_id, "agent-1", "task-call-1")
            .await
            .unwrap();
        assert!(store
            .consumed_subagent_completion(&conversation_id, "agent-1", "task-call-1")
            .await
            .unwrap());
    }
}
