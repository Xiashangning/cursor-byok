//! Creates, loads, and updates Conversations.
use sqlx::{Row, Sqlite, Transaction};

use crate::{
    model::{CheckpointId, ConversationId, RunId},
    Error, Result,
};

use super::{now_ms, Store};

impl Store {
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

    pub(crate) async fn conversation_model_selection(
        &self,
        conversation_id: &ConversationId,
    ) -> Result<Option<crate::model::ModelSelection>> {
        let value = sqlx::query_scalar::<_, Option<String>>(
            "SELECT model_selection FROM conversations WHERE conversation_id = ?",
        )
        .bind(conversation_id.as_str())
        .fetch_optional(&self.pool)
        .await?
        .flatten();
        value
            .map(|value| serde_json::from_str(&value).map_err(Error::from))
            .transpose()
    }

    pub(crate) async fn set_conversation_model_selection(
        &self,
        conversation_id: &ConversationId,
        selection: Option<&crate::model::ModelSelection>,
    ) -> Result<()> {
        let model_selection = selection.map(serde_json::to_string).transpose()?;
        let _write = self.writes.lock().await;
        sqlx::query(
            "UPDATE conversations SET model_selection = ?, updated_at_ms = ?
             WHERE conversation_id = ?",
        )
        .bind(model_selection)
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
    async fn official_selection_round_trips() {
        let store = Store::connect("sqlite::memory:").await.unwrap();
        let id = ConversationId::new("official");
        store.ensure_conversation(&id).await.unwrap();
        let selection = crate::model::ModelSelection::Official {
            model: "unknown-official-model".into(),
            parameters: Default::default(),
        };
        store
            .set_conversation_model_selection(&id, Some(&selection))
            .await
            .unwrap();
        assert_eq!(
            store.conversation_model_selection(&id).await.unwrap(),
            Some(selection)
        );
    }

    #[tokio::test]
    async fn conversation_model_selection_round_trips_and_clears() {
        let store = Store::connect("sqlite::memory:").await.unwrap();
        let conversation_id = ConversationId::new("conversation-1");
        store.ensure_conversation(&conversation_id).await.unwrap();

        assert_eq!(
            store
                .conversation_model_selection(&conversation_id)
                .await
                .unwrap(),
            None
        );
        let selection: crate::model::ModelSelection = serde_json::from_value(serde_json::json!({"kind":"local","id":"abcd1234","parameters":{"context":"272k","reasoning":"high","fast":false}})).unwrap();
        store
            .set_conversation_model_selection(&conversation_id, Some(&selection))
            .await
            .unwrap();
        assert_eq!(
            store
                .conversation_model_selection(&conversation_id)
                .await
                .unwrap()
                .as_ref(),
            Some(&selection)
        );
        store
            .set_conversation_model_selection(&conversation_id, None)
            .await
            .unwrap();
        assert_eq!(
            store
                .conversation_model_selection(&conversation_id)
                .await
                .unwrap(),
            None
        );
    }
}
