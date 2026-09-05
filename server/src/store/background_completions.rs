//! Atomically arbitrates terminal background-task completion delivery.

use sqlx::{Row, Sqlite, Transaction};

use crate::{
    model::{ConversationId, TerminalCompletion},
    Error, Result,
};

use super::{now_ms, Store};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CompletionDisposition {
    Consumed,
    Projected,
}

impl CompletionDisposition {
    fn as_str(self) -> &'static str {
        match self {
            Self::Consumed => "consumed",
            Self::Projected => "projected",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CompletionClaim {
    Acquired,
    AlreadyConsumed,
    AlreadyProjected,
}

impl Store {
    /// Fast retry check. The actual decision is repeated inside the canonical
    /// message commit transaction.
    pub(crate) async fn background_completions_processed(
        &self,
        conversation_id: &ConversationId,
        completions: &[TerminalCompletion],
    ) -> Result<bool> {
        for completion in completions {
            let Some(row) = sqlx::query(
                "SELECT terminal_status, disposition, payload_digest, runtime_event_id
                 FROM background_completion_claims
                 WHERE conversation_id = ? AND task_kind = ? AND task_id = ?",
            )
            .bind(conversation_id.as_str())
            .bind(&completion.kind)
            .bind(&completion.task_id)
            .fetch_optional(&self.pool)
            .await?
            else {
                return Ok(false);
            };
            let compare_payload = row.get::<&str, _>(1) == "projected";
            validate_existing(&row, completion, compare_payload)?;
        }
        Ok(!completions.is_empty())
    }

    pub(crate) async fn claim_completion_tx(
        tx: &mut Transaction<'_, Sqlite>,
        conversation_id: &ConversationId,
        completion: &TerminalCompletion,
        disposition: CompletionDisposition,
    ) -> Result<CompletionClaim> {
        if let Some(row) = sqlx::query(
            "SELECT terminal_status, disposition, payload_digest, runtime_event_id
             FROM background_completion_claims
             WHERE conversation_id = ? AND task_kind = ? AND task_id = ?",
        )
        .bind(conversation_id.as_str())
        .bind(&completion.kind)
        .bind(&completion.task_id)
        .fetch_optional(&mut **tx)
        .await?
        {
            let existing_disposition: &str = row.get(1);
            validate_existing(
                &row,
                completion,
                disposition == CompletionDisposition::Projected
                    && existing_disposition == "projected",
            )?;
            return Ok(match existing_disposition {
                "consumed" => CompletionClaim::AlreadyConsumed,
                "projected" => CompletionClaim::AlreadyProjected,
                value => {
                    return Err(Error::Store(format!(
                        "invalid background completion disposition: {value}"
                    )))
                }
            });
        }

        let (payload_digest, runtime_event_id) = match disposition {
            CompletionDisposition::Consumed => (None, None),
            CompletionDisposition::Projected => (
                completion.payload_digest.as_deref(),
                Some(completion.event_id.as_str()),
            ),
        };
        sqlx::query(
            "INSERT INTO background_completion_claims(
                 conversation_id, task_kind, task_id, terminal_status, disposition,
                 payload_digest, runtime_event_id, created_at_ms
             ) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(conversation_id.as_str())
        .bind(&completion.kind)
        .bind(&completion.task_id)
        .bind(&completion.status)
        .bind(disposition.as_str())
        .bind(payload_digest)
        .bind(runtime_event_id)
        .bind(now_ms())
        .execute(&mut **tx)
        .await?;
        Ok(CompletionClaim::Acquired)
    }
}

fn validate_existing(
    row: &sqlx::sqlite::SqliteRow,
    completion: &TerminalCompletion,
    compare_projection_payload: bool,
) -> Result<()> {
    let status: &str = row.get(0);
    let disposition: &str = row.get(1);
    let payload_digest: Option<&str> = row.get(2);
    let event_id: Option<&str> = row.get(3);
    let payload_conflict = compare_projection_payload
        && (payload_digest != completion.payload_digest.as_deref()
            || event_id != Some(completion.event_id.as_str()));
    if status != completion.status || payload_conflict {
        return Err(Error::Store(format!(
            "conflicting terminal completion for {} task {}: stored {status}/{disposition}, received {}",
            completion.kind, completion.task_id, completion.status
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn completion(status: &str, digest: &str) -> TerminalCompletion {
        TerminalCompletion {
            task_id: "task-1".into(),
            kind: "subagent".into(),
            status: status.into(),
            payload_digest: Some(digest.into()),
            event_id: "background-completed:event-1".into(),
        }
    }

    #[tokio::test]
    async fn one_terminal_path_atomically_acquires_a_lifecycle() {
        let directory = tempfile::tempdir().unwrap();
        let url = format!("sqlite://{}", directory.path().join("test.db").display());
        let store = Store::connect(&url).await.unwrap();
        let conversation_id = ConversationId::new("conversation-1");
        store.ensure_conversation(&conversation_id).await.unwrap();

        let _write = store.writes.lock().await;
        let mut tx = store.pool.begin_with("BEGIN IMMEDIATE").await.unwrap();
        assert_eq!(
            Store::claim_completion_tx(
                &mut tx,
                &conversation_id,
                &completion("success", "digest-1"),
                CompletionDisposition::Consumed,
            )
            .await
            .unwrap(),
            CompletionClaim::Acquired
        );
        tx.commit().await.unwrap();
        drop(_write);

        assert!(store
            .background_completions_processed(
                &conversation_id,
                &[completion("success", "digest-1")]
            )
            .await
            .unwrap());
    }

    #[tokio::test]
    async fn concurrent_consumption_and_projection_have_one_winner() {
        async fn claim(
            store: Store,
            conversation_id: ConversationId,
            disposition: CompletionDisposition,
        ) -> CompletionClaim {
            let _write = store.writes.lock().await;
            let mut tx = store.pool.begin_with("BEGIN IMMEDIATE").await.unwrap();
            let claim = Store::claim_completion_tx(
                &mut tx,
                &conversation_id,
                &completion("success", "digest-1"),
                disposition,
            )
            .await
            .unwrap();
            tx.commit().await.unwrap();
            claim
        }

        let directory = tempfile::tempdir().unwrap();
        let url = format!("sqlite://{}", directory.path().join("test.db").display());
        let store = Store::connect(&url).await.unwrap();
        let second_store = Store::connect(&url).await.unwrap();
        let conversation_id = ConversationId::new("conversation-1");
        store.ensure_conversation(&conversation_id).await.unwrap();

        let (consumed, projected) = tokio::join!(
            claim(
                store,
                conversation_id.clone(),
                CompletionDisposition::Consumed,
            ),
            claim(
                second_store,
                conversation_id,
                CompletionDisposition::Projected,
            )
        );
        assert_eq!(
            usize::from(consumed == CompletionClaim::Acquired)
                + usize::from(projected == CompletionClaim::Acquired),
            1
        );
    }

    #[tokio::test]
    async fn conflicting_terminal_status_is_an_error() {
        let directory = tempfile::tempdir().unwrap();
        let url = format!("sqlite://{}", directory.path().join("test.db").display());
        let store = Store::connect(&url).await.unwrap();
        let conversation_id = ConversationId::new("conversation-1");
        store.ensure_conversation(&conversation_id).await.unwrap();

        let _write = store.writes.lock().await;
        let mut tx = store.pool.begin_with("BEGIN IMMEDIATE").await.unwrap();
        Store::claim_completion_tx(
            &mut tx,
            &conversation_id,
            &completion("success", "digest-1"),
            CompletionDisposition::Projected,
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
        drop(_write);

        let error = store
            .background_completions_processed(&conversation_id, &[completion("error", "digest-1")])
            .await
            .unwrap_err();
        assert!(error
            .to_string()
            .contains("conflicting terminal completion"));
    }
}
