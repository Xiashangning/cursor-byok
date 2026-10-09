//! Persists Cursor request traces and artifacts.
use sqlx::{Row, Sqlite, Transaction};

use crate::{
    model::{CursorRunTraceArtifact, CursorRunTraceSummary},
    Result,
};

use super::{now_ms, BlobId, Store};

#[derive(Clone, Debug)]
pub(crate) struct BufferedCursorTraceChunk {
    pub(crate) source: String,
    pub(crate) data: Vec<u8>,
}

impl BufferedCursorTraceChunk {
    pub(crate) fn new(source: &str, data: &[u8]) -> Self {
        Self {
            source: source.into(),
            data: data.to_vec(),
        }
    }
}

impl Store {
    pub async fn start_cursor_trace_if_detailed(
        &self,
        request_id: &str,
        conversation_id: Option<&str>,
        route: &str,
        model_id: Option<&str>,
    ) -> Result<bool> {
        // 读路径不占写锁:进行中的生命周期与未开启详细日志都不会写库。
        let status = self.cursor_trace_status(request_id).await?;
        // 进行中的行属于当前生命周期;已终结的行在 request_id 复用时重置为新生命周期。
        if status.as_deref() == Some("running") {
            return Ok(true);
        }
        if !self.detailed_logging().await? {
            return Ok(false);
        }
        let _write = self.writes.lock().await;
        // 锁内复查:并发启动可能已建行。
        let status = self.cursor_trace_status(request_id).await?;
        if status.as_deref() == Some("running") {
            return Ok(true);
        }
        if status.is_none() {
            sqlx::query(
                "INSERT INTO cursor_run_traces(
                    request_id, conversation_id, route, model_id, status, received_at_ms
                 ) VALUES (?, ?, ?, ?, 'running', ?)",
            )
            .bind(request_id)
            .bind(conversation_id)
            .bind(route)
            .bind(model_id)
            .bind(now_ms())
            .execute(&self.pool)
            .await?;
            return Ok(true);
        }
        // request_id 复用:旧生命周期的附件随行一起重置,指标从零重新累计;
        // 失去锚点的 blob 由 prune_unanchored_blobs 回收。
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        sqlx::query("DELETE FROM cursor_run_trace_artifacts WHERE request_id = ?")
            .bind(request_id)
            .execute(&mut *tx)
            .await?;
        sqlx::query(
            "UPDATE cursor_run_traces
             SET conversation_id = ?, route = ?, model_id = ?, status = 'running',
                 request_bytes = 0, response_bytes = 0, response_event_count = 0,
                 http_status = NULL, received_at_ms = ?, first_response_at_ms = NULL,
                 finished_at_ms = NULL, error_message = NULL
             WHERE request_id = ?",
        )
        .bind(conversation_id)
        .bind(route)
        .bind(model_id)
        .bind(now_ms())
        .bind(request_id)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(true)
    }

    pub(crate) async fn cursor_trace_status(&self, request_id: &str) -> Result<Option<String>> {
        Ok(
            sqlx::query_scalar("SELECT status FROM cursor_run_traces WHERE request_id = ?")
                .bind(request_id)
                .fetch_optional(&self.pool)
                .await?,
        )
    }

    pub async fn append_cursor_trace_request(
        &self,
        request_id: &str,
        artifact_type: &str,
        source: &str,
        data: &[u8],
        metadata: &serde_json::Value,
    ) -> Result<()> {
        let metadata_json = serde_json::to_string(metadata)?;
        let blob_id = BlobId::digest(data);
        let _write = self.writes.lock().await;
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        Self::put_blob_tx(&mut tx, &blob_id, data, &[]).await?;
        Self::link_cursor_trace_artifact_tx(
            &mut tx,
            request_id,
            artifact_type,
            source,
            &blob_id,
            &metadata_json,
        )
        .await?;
        sqlx::query(
            "UPDATE cursor_run_traces
             SET request_bytes = request_bytes + ? WHERE request_id = ?",
        )
        .bind(as_i64(data.len()))
        .bind(request_id)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn append_cursor_trace_artifact(
        &self,
        request_id: &str,
        artifact_type: &str,
        source: &str,
        data: &[u8],
        metadata: &serde_json::Value,
    ) -> Result<()> {
        let metadata_json = serde_json::to_string(metadata)?;
        let blob_id = BlobId::digest(data);
        let _write = self.writes.lock().await;
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        Self::put_blob_tx(&mut tx, &blob_id, data, &[]).await?;
        Self::link_cursor_trace_artifact_tx(
            &mut tx,
            request_id,
            artifact_type,
            source,
            &blob_id,
            &metadata_json,
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn link_cursor_trace_artifact(
        &self,
        request_id: &str,
        artifact_type: &str,
        source: &str,
        blob_id: &BlobId,
        metadata: &serde_json::Value,
    ) -> Result<()> {
        let metadata_json = serde_json::to_string(metadata)?;
        let _write = self.writes.lock().await;
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        Self::link_cursor_trace_artifact_tx(
            &mut tx,
            request_id,
            artifact_type,
            source,
            blob_id,
            &metadata_json,
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }

    async fn link_cursor_trace_artifact_tx(
        tx: &mut Transaction<'_, Sqlite>,
        request_id: &str,
        artifact_type: &str,
        source: &str,
        blob_id: &BlobId,
        metadata_json: &str,
    ) -> Result<()> {
        let next: i64 = sqlx::query_scalar(
            "SELECT COALESCE(MAX(seq), -1) + 1
             FROM cursor_run_trace_artifacts WHERE request_id = ?",
        )
        .bind(request_id)
        .fetch_one(&mut **tx)
        .await?;
        sqlx::query(
            "INSERT INTO cursor_run_trace_artifacts(
                request_id, seq, artifact_type, source, blob_id, metadata_json, created_at_ms
             ) VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(request_id)
        .bind(next)
        .bind(artifact_type)
        .bind(source)
        .bind(blob_id.as_bytes().as_slice())
        .bind(metadata_json)
        .bind(now_ms())
        .execute(&mut **tx)
        .await?;
        Ok(())
    }

    pub async fn start_cursor_trace_response(&self, request_id: &str, status: u16) -> Result<()> {
        let now = now_ms();
        let _write = self.writes.lock().await;
        sqlx::query(
            "UPDATE cursor_run_traces
             SET http_status = ?, first_response_at_ms = COALESCE(first_response_at_ms, ?)
             WHERE request_id = ? AND status = 'running'",
        )
        .bind(status as i64)
        .bind(now)
        .bind(request_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub(crate) async fn add_cursor_trace_response_chunks(
        &self,
        request_id: &str,
        chunks: &[BufferedCursorTraceChunk],
    ) -> Result<()> {
        if chunks.is_empty() {
            return Ok(());
        }
        let response_bytes = chunks.iter().map(|chunk| chunk.data.len()).sum::<usize>();
        let _write = self.writes.lock().await;
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        for chunk in chunks {
            let metadata_json =
                serde_json::to_string(&serde_json::json!({"byte_count": chunk.data.len()}))?;
            let blob_id = BlobId::digest(&chunk.data);
            Self::put_blob_tx(&mut tx, &blob_id, &chunk.data, &[]).await?;
            Self::link_cursor_trace_artifact_tx(
                &mut tx,
                request_id,
                "run_sse_chunk",
                &chunk.source,
                &blob_id,
                &metadata_json,
            )
            .await?;
        }
        sqlx::query(
            "UPDATE cursor_run_traces
             SET response_bytes = response_bytes + ?,
                 response_event_count = response_event_count + ?,
                 first_response_at_ms = COALESCE(first_response_at_ms, ?)
             WHERE request_id = ?",
        )
        .bind(as_i64(response_bytes))
        .bind(chunks.len() as i64)
        .bind(now_ms())
        .bind(request_id)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn finish_cursor_trace(&self, request_id: &str, error: Option<&str>) -> Result<()> {
        let _write = self.writes.lock().await;
        sqlx::query(
            "UPDATE cursor_run_traces
             SET status = ?, finished_at_ms = ?, error_message = ?
             WHERE request_id = ? AND status = 'running'",
        )
        .bind(if error.is_some() {
            "error"
        } else {
            "completed"
        })
        .bind(now_ms())
        .bind(error)
        .bind(request_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn cursor_trace(&self, request_id: &str) -> Result<Option<CursorRunTraceSummary>> {
        sqlx::query(
            "SELECT t.request_id, t.conversation_id, t.route, t.model_id, t.status,
                    t.request_bytes, t.response_bytes, t.response_event_count, t.http_status,
                    t.received_at_ms, t.first_response_at_ms, t.finished_at_ms, t.error_message,
                    EXISTS(
                        SELECT 1 FROM cursor_run_trace_artifacts a
                        WHERE a.request_id = t.request_id
                    ) AS detailed
             FROM cursor_run_traces t WHERE t.request_id = ?",
        )
        .bind(request_id)
        .fetch_optional(&self.pool)
        .await?
        .map(trace_from_row)
        .transpose()
    }

    /// Cursor traces that need their own Calls-page row. Official runs have no
    /// local provider call; a local trace is listed only after it ends without
    /// ever linking one. While it runs, either its provider call row (or no row
    /// yet) represents it, so the list never swaps a trace row for a call row.
    pub(crate) async fn standalone_cursor_traces(
        &self,
        limit: i64,
    ) -> Result<Vec<CursorRunTraceSummary>> {
        let rows = sqlx::query(
            "SELECT t.request_id, t.conversation_id, t.route, t.model_id, t.status,
                    t.request_bytes, t.response_bytes, t.response_event_count, t.http_status,
                    t.received_at_ms, t.first_response_at_ms, t.finished_at_ms, t.error_message,
                    EXISTS(
                        SELECT 1 FROM cursor_run_trace_artifacts a
                        WHERE a.request_id = t.request_id
                    ) AS detailed
             FROM cursor_run_traces t
             WHERE t.route = 'cursor_official'
                OR (
                    t.status <> 'running'
                    AND NOT EXISTS (
                        SELECT 1 FROM runs r
                        JOIN llm_calls c ON c.run_id = r.run_id
                        WHERE r.cursor_request_id = t.request_id
                    )
                )
             ORDER BY t.received_at_ms DESC LIMIT ?",
        )
        .bind(limit.clamp(1, 500))
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(trace_from_row).collect()
    }

    pub async fn cursor_trace_artifacts(
        &self,
        request_id: &str,
    ) -> Result<Vec<CursorRunTraceArtifact>> {
        let rows = sqlx::query(
            "SELECT a.seq, a.artifact_type, a.source, a.metadata_json,
                    a.created_at_ms, b.data
             FROM cursor_run_trace_artifacts a
             JOIN blobs b ON b.blob_id = a.blob_id
             WHERE a.request_id = ? ORDER BY a.seq",
        )
        .bind(request_id)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|row| {
                Ok(CursorRunTraceArtifact {
                    seq: row.try_get("seq")?,
                    artifact_type: row.try_get("artifact_type")?,
                    source: row.try_get("source")?,
                    metadata: serde_json::from_str(row.try_get("metadata_json")?)?,
                    created_at_ms: row.try_get("created_at_ms")?,
                    data: row.try_get("data")?,
                })
            })
            .collect()
    }
}

fn trace_from_row(row: sqlx::sqlite::SqliteRow) -> Result<CursorRunTraceSummary> {
    Ok(CursorRunTraceSummary {
        request_id: row.try_get("request_id")?,
        conversation_id: row.try_get("conversation_id")?,
        route: row.try_get("route")?,
        model_id: row.try_get("model_id")?,
        status: row.try_get("status")?,
        request_bytes: row.try_get("request_bytes")?,
        response_bytes: row.try_get("response_bytes")?,
        response_event_count: row.try_get("response_event_count")?,
        http_status: row.try_get("http_status")?,
        received_at_ms: row.try_get("received_at_ms")?,
        first_response_at_ms: row.try_get("first_response_at_ms")?,
        finished_at_ms: row.try_get("finished_at_ms")?,
        error_message: row.try_get("error_message")?,
        detailed: row.try_get("detailed")?,
    })
}

fn as_i64(value: usize) -> i64 {
    value.min(i64::MAX as usize) as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn reused_request_id_starts_a_new_lifecycle() {
        let store = Store::connect("sqlite::memory:").await.unwrap();
        store.set_detailed_logging(true).await.unwrap();
        assert!(store
            .start_cursor_trace_if_detailed("req-1", Some("conv-1"), "local_byok", Some("model"))
            .await
            .unwrap());
        store
            .append_cursor_trace_request(
                "req-1",
                "bidi_request",
                "cursor_client",
                b"payload",
                &serde_json::json!({}),
            )
            .await
            .unwrap();
        store.finish_cursor_trace("req-1", None).await.unwrap();
        assert!(
            store.cursor_trace("req-1").await.unwrap().unwrap().detailed,
            "附件存在时详细记录为真"
        );

        // Cursor 客户端复用同一 request_id:旧行重置为新生命周期,旧附件与指标清零。
        assert!(store
            .start_cursor_trace_if_detailed("req-1", Some("conv-2"), "cursor_official", None)
            .await
            .unwrap());
        let trace = store.cursor_trace("req-1").await.unwrap().unwrap();
        assert_eq!(trace.status, "running");
        assert_eq!(trace.conversation_id.as_deref(), Some("conv-2"));
        assert_eq!(trace.route, "cursor_official");
        assert_eq!(trace.request_bytes, 0);
        assert!(trace.finished_at_ms.is_none());
        assert!(!trace.detailed, "旧附件随行重置后详细记录为假");
        assert!(store
            .cursor_trace_artifacts("req-1")
            .await
            .unwrap()
            .is_empty());

        // 新生命周期不再被旧的终结状态卡住,可以正常结束。
        store
            .finish_cursor_trace("req-1", Some("boom"))
            .await
            .unwrap();
        let trace = store.cursor_trace("req-1").await.unwrap().unwrap();
        assert_eq!(trace.status, "error");
        assert_eq!(trace.error_message.as_deref(), Some("boom"));
    }
}
