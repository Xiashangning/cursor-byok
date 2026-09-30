//! Creates, loads, and updates Conversations.
use sqlx::{Row, Sqlite, Transaction};

use crate::{
    model::{CheckpointId, Conversation, ConversationId, RunId},
    Error, Result,
};

use super::{now_ms, Store};

impl Store {
    pub async fn conversation(
        &self,
        conversation_id: &ConversationId,
    ) -> Result<Option<Conversation>> {
        let row = sqlx::query(
            "SELECT current_checkpoint_id, active_run_id
             FROM conversations WHERE conversation_id = ?",
        )
        .bind(conversation_id.as_str())
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|row| Conversation {
            conversation_id: conversation_id.clone(),
            current_checkpoint_id: CheckpointId(row.get(0)),
            active_run_id: row.get::<Option<String>, _>(1).map(RunId),
        }))
    }

    /// 会话行是否存在;子任务据此区分首次运行与续接。
    pub(crate) async fn conversation_exists(
        &self,
        conversation_id: &ConversationId,
    ) -> Result<bool> {
        Ok(sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS(SELECT 1 FROM conversations WHERE conversation_id = ?)",
        )
        .bind(conversation_id.as_str())
        .fetch_one(&self.pool)
        .await?)
    }

    pub(crate) async fn conversation_model_variant(
        &self,
        conversation_id: &ConversationId,
    ) -> Result<Option<String>> {
        Ok(sqlx::query_scalar::<_, Option<String>>(
            "SELECT model_variant FROM conversations WHERE conversation_id = ?",
        )
        .bind(conversation_id.as_str())
        .fetch_optional(&self.pool)
        .await?
        .flatten())
    }

    pub(crate) async fn set_conversation_model_variant(
        &self,
        conversation_id: &ConversationId,
        model_variant: Option<&str>,
    ) -> Result<()> {
        let _write = self.writes.lock().await;
        sqlx::query(
            "UPDATE conversations SET model_variant = ?, updated_at_ms = ?
             WHERE conversation_id = ?",
        )
        .bind(model_variant)
        .bind(now_ms())
        .bind(conversation_id.as_str())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub(crate) async fn ensure_conversation_tx(
        tx: &mut Transaction<'_, Sqlite>,
        conversation_id: &ConversationId,
    ) -> Result<CheckpointId> {
        sqlx::query(
            "INSERT OR IGNORE INTO conversations(conversation_id, updated_at_ms) VALUES (?, ?)",
        )
        .bind(conversation_id.as_str())
        .bind(now_ms())
        .execute(&mut **tx)
        .await?;

        let current: Option<i64> = sqlx::query_scalar(
            "SELECT current_checkpoint_id FROM conversations WHERE conversation_id = ?",
        )
        .bind(conversation_id.as_str())
        .fetch_one(&mut **tx)
        .await?;
        if let Some(current) = current {
            return Ok(CheckpointId(current));
        }

        let digest = super::checkpoints::message_digest(&[])?;
        let root = sqlx::query(
            "INSERT INTO conversation_checkpoints
             (conversation_id, parent_checkpoint_id, state_digest, created_at_ms)
             VALUES (?, NULL, ?, ?)",
        )
        .bind(conversation_id.as_str())
        .bind(digest.as_slice())
        .bind(now_ms())
        .execute(&mut **tx)
        .await?
        .last_insert_rowid();
        sqlx::query(
            "UPDATE conversations SET current_checkpoint_id = ?, updated_at_ms = ?
             WHERE conversation_id = ? AND current_checkpoint_id IS NULL",
        )
        .bind(root)
        .bind(now_ms())
        .bind(conversation_id.as_str())
        .execute(&mut **tx)
        .await?;
        Ok(CheckpointId(root))
    }

    pub(crate) async fn require_active_head_tx(
        tx: &mut Transaction<'_, Sqlite>,
        conversation_id: &ConversationId,
        run_id: &RunId,
        expected: CheckpointId,
    ) -> Result<()> {
        let row = sqlx::query(
            "SELECT current_checkpoint_id, active_run_id FROM conversations WHERE conversation_id = ?",
        )
        .bind(conversation_id.as_str())
        .fetch_optional(&mut **tx)
        .await?;
        match row {
            Some(row)
                if row.get::<Option<i64>, _>(0) == Some(expected.0)
                    && row.get::<Option<&str>, _>(1) == Some(run_id.as_str()) =>
            {
                Ok(())
            }
            _ => Err(Error::Store(format!(
                "run {run_id} no longer owns conversation {conversation_id} at checkpoint {expected}"
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn conversation_model_variant_round_trips_and_clears() {
        let store = Store::connect("sqlite::memory:").await.unwrap();
        let conversation_id = ConversationId::new("conversation-1");
        store.ensure_conversation(&conversation_id).await.unwrap();

        assert_eq!(
            store
                .conversation_model_variant(&conversation_id)
                .await
                .unwrap(),
            None
        );
        store
            .set_conversation_model_variant(&conversation_id, Some("model-272k-high"))
            .await
            .unwrap();
        assert_eq!(
            store
                .conversation_model_variant(&conversation_id)
                .await
                .unwrap()
                .as_deref(),
            Some("model-272k-high")
        );
        store
            .set_conversation_model_variant(&conversation_id, None)
            .await
            .unwrap();
        assert_eq!(
            store
                .conversation_model_variant(&conversation_id)
                .await
                .unwrap(),
            None
        );
    }
}
