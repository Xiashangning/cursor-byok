//! Persists Run ownership, status, and provider call progress.
use sqlx::Row;

use crate::{
    model::{CheckpointId, ConversationId, PreparedRun, RunId, RunKind},
    Error, Result,
};

use super::{now_ms, Store};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunStatus {
    Running,
    Completed,
    Cancelled,
    Failed,
}

impl RunStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Cancelled => "cancelled",
            Self::Failed => "failed",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClaimedRun {
    pub run_id: RunId,
    pub conversation_id: ConversationId,
    pub head_checkpoint_id: CheckpointId,
}

impl Store {
    /// Resolve the run that dispatched this call, including finished runs. A Cursor
    /// request may own multiple runs; its latest run need not own the parent call.
    pub(crate) async fn parent_tool_call_run(
        &self,
        request_id: &str,
        tool_call_id: &str,
    ) -> Result<Option<(RunId, serde_json::Value)>> {
        let row: Option<(String, String)> = sqlx::query_as(
            "SELECT r.run_id, c.arguments_json
             FROM runs r
             JOIN tool_rounds t ON t.run_id = r.run_id
             JOIN tool_round_calls c ON c.round_id = t.round_id
             WHERE r.cursor_request_id = ? AND c.call_id = ?
             ORDER BY t.created_at_ms DESC, t.rowid DESC LIMIT 1",
        )
        .bind(request_id)
        .bind(tool_call_id)
        .fetch_optional(&self.pool)
        .await?;
        row.map(|(run_id, arguments)| Ok((RunId::new(run_id), serde_json::from_str(&arguments)?)))
            .transpose()
    }

    pub async fn claim_run(&self, prepared: &PreparedRun) -> Result<ClaimedRun> {
        let _write = self.writes.lock().await;
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let now = now_ms();
        Self::ensure_conversation_tx(&mut tx, &prepared.conversation_id).await?;
        let belongs: bool = sqlx::query_scalar(
            "SELECT EXISTS(
                SELECT 1 FROM conversation_checkpoints
                WHERE checkpoint_id = ? AND conversation_id = ?
             )",
        )
        .bind(prepared.base_checkpoint_id.0)
        .bind(prepared.conversation_id.as_str())
        .fetch_one(&mut *tx)
        .await?;
        if !belongs {
            return Err(Error::Store(format!(
                "base checkpoint {} does not belong to conversation {}",
                prepared.base_checkpoint_id, prepared.conversation_id
            )));
        }

        let replaced: Option<String> =
            sqlx::query_scalar("SELECT active_run_id FROM conversations WHERE conversation_id = ?")
                .bind(prepared.conversation_id.as_str())
                .fetch_one(&mut *tx)
                .await?;
        if let Some(replaced) = replaced.as_deref() {
            if replaced != prepared.run_id.as_str() {
                sqlx::query(
                    "UPDATE runs SET status = 'cancelled', updated_at_ms = ?
                     WHERE run_id = ? AND status = 'running'",
                )
                .bind(now)
                .bind(replaced)
                .execute(&mut *tx)
                .await?;
                sqlx::query(
                    "UPDATE llm_calls SET status = 'cancelled', finished_at_ms = ?,
                     duration_ms = MAX(0, ? - created_at_ms)
                     WHERE run_id = ? AND status = 'running'",
                )
                .bind(now)
                .bind(now)
                .bind(replaced)
                .execute(&mut *tx)
                .await?;
            }
        }

        let (parent_run_id, parent_tool_call_id, run_kind, subagent_kind) =
            run_kind_columns(&prepared.kind);
        sqlx::query(
            "INSERT INTO runs
             (run_id, cursor_request_id, conversation_id, base_checkpoint_id, head_checkpoint_id,
              parent_run_id, parent_tool_call_id, run_kind, subagent_kind,
              status, created_at_ms, updated_at_ms)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, 'running', ?, ?)",
        )
        .bind(prepared.run_id.as_str())
        .bind(prepared.cursor_request_id.as_deref())
        .bind(prepared.conversation_id.as_str())
        .bind(prepared.base_checkpoint_id.0)
        .bind(prepared.base_checkpoint_id.0)
        .bind(parent_run_id)
        .bind(parent_tool_call_id)
        .bind(run_kind)
        .bind(subagent_kind)
        .bind(now)
        .bind(now)
        .execute(&mut *tx)
        .await?;

        sqlx::query(
            "UPDATE conversations
             SET current_checkpoint_id = ?, active_run_id = ?, updated_at_ms = ?
             WHERE conversation_id = ?",
        )
        .bind(prepared.base_checkpoint_id.0)
        .bind(prepared.run_id.as_str())
        .bind(now)
        .bind(prepared.conversation_id.as_str())
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(ClaimedRun {
            run_id: prepared.run_id.clone(),
            conversation_id: prepared.conversation_id.clone(),
            head_checkpoint_id: prepared.base_checkpoint_id,
        })
    }

    /// 一次本地 Run 关联的 Cursor request id;追踪与 provider 调用靠它关联。
    pub(crate) async fn run_cursor_request_id(&self, run_id: &str) -> Result<Option<String>> {
        let request_id: Option<Option<String>> =
            sqlx::query_scalar("SELECT cursor_request_id FROM runs WHERE run_id = ?")
                .bind(run_id)
                .fetch_optional(&self.pool)
                .await?;
        Ok(request_id.flatten())
    }

    pub async fn active_run_for_cursor_request(
        &self,
        cursor_request_id: &str,
    ) -> Result<Option<RunId>> {
        let run_id: Option<String> = sqlx::query_scalar(
            "SELECT run_id FROM runs
             WHERE cursor_request_id = ? AND status = 'running'
             ORDER BY created_at_ms DESC
             LIMIT 1",
        )
        .bind(cursor_request_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(run_id.map(RunId))
    }

    pub async fn begin_provider_call(&self, run_id: &RunId) -> Result<u64> {
        let _write = self.writes.lock().await;
        let index: Option<i64> = sqlx::query_scalar(
            "UPDATE runs SET provider_call_index = provider_call_index + 1, updated_at_ms = ?
             WHERE run_id = ? AND status = 'running'
             RETURNING provider_call_index",
        )
        .bind(now_ms())
        .bind(run_id.as_str())
        .fetch_optional(&self.pool)
        .await?;
        index
            .map(|index| index as u64)
            .ok_or_else(|| Error::Store(format!("run is not active: {run_id}")))
    }

    pub async fn finish_run(
        &self,
        run_id: &RunId,
        status: RunStatus,
        failure: Option<(&str, &str)>,
    ) -> Result<bool> {
        let _write = self.writes.lock().await;
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let row = sqlx::query(
            "SELECT conversation_id, status, failure_category, failure_summary
             FROM runs WHERE run_id = ?",
        )
        .bind(run_id.as_str())
        .fetch_optional(&mut *tx)
        .await?;
        let Some(row) = row else {
            return Err(Error::RunNotFound(run_id.to_string()));
        };
        let conversation_id: String = row.get("conversation_id");
        let current_status: String = row.get("status");
        let (requested_category, requested_summary) = failure.unzip();
        let terminal_status = if current_status == "running" {
            status.as_str()
        } else {
            current_status.as_str()
        };
        let stored_category: Option<String> = row.get("failure_category");
        let stored_summary: Option<String> = row.get("failure_summary");
        let (category, summary) = if current_status == "running" {
            (requested_category, requested_summary)
        } else {
            (stored_category.as_deref(), stored_summary.as_deref())
        };
        let now = now_ms();
        sqlx::query(
            "UPDATE runs SET status = ?, failure_category = ?,
             failure_summary = ?, updated_at_ms = ?
             WHERE run_id = ? AND status = 'running'",
        )
        .bind(status.as_str())
        .bind(category)
        .bind(summary)
        .bind(now)
        .bind(run_id.as_str())
        .execute(&mut *tx)
        .await?;
        let (call_status, call_error_kind, call_error_message) = match terminal_status {
            "cancelled" => ("cancelled", None, None),
            "failed" => ("error", category, summary),
            "completed" => (
                "error",
                Some("internal"),
                Some("Run completed before LLM call reached a terminal state"),
            ),
            value => {
                return Err(Error::Store(format!(
                    "cannot finish LLM calls for non-terminal Run status: {value}"
                )))
            }
        };
        sqlx::query(
            "UPDATE llm_calls SET status = ?, finished_at_ms = ?,
             duration_ms = MAX(0, ? - created_at_ms), error_kind = ?, error_message = ?
             WHERE run_id = ? AND status = 'running'",
        )
        .bind(call_status)
        .bind(now)
        .bind(now)
        .bind(call_error_kind)
        .bind(call_error_message)
        .bind(run_id.as_str())
        .execute(&mut *tx)
        .await?;
        let released = sqlx::query(
            "UPDATE conversations SET active_run_id = NULL, updated_at_ms = ?
             WHERE conversation_id = ? AND active_run_id = ?",
        )
        .bind(now)
        .bind(conversation_id)
        .bind(run_id.as_str())
        .execute(&mut *tx)
        .await?
        .rows_affected()
            == 1;
        tx.commit().await?;
        Ok(released)
    }
}

fn run_kind_columns(kind: &RunKind) -> (Option<&str>, Option<&str>, &'static str, Option<String>) {
    match kind {
        RunKind::Root => (None, None, "root", None),
        RunKind::Subagent {
            parent_run_id,
            parent_tool_call_id,
            kind,
            ..
        } => (
            parent_run_id.as_ref().map(RunId::as_str),
            parent_tool_call_id.as_deref(),
            "subagent",
            Some(match kind {
                crate::model::SubagentKind::GeneralPurpose => "generalPurpose".into(),
                crate::model::SubagentKind::Named(name) => name.clone(),
            }),
        ),
    }
}
