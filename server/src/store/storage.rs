//! Persists content-addressed blobs and their edges.
//! Storage accounting and cleanup for disposable observability data.

use crate::Result;

use super::Store;

impl Store {
    /// 数据库文件占用（页数 × 页大小）。
    pub async fn database_bytes(&self) -> Result<i64> {
        let page_size: i64 = sqlx::query_scalar("PRAGMA page_size")
            .fetch_one(&self.pool)
            .await?;
        let page_count: i64 = sqlx::query_scalar("PRAGMA page_count")
            .fetch_one(&self.pool)
            .await?;
        Ok(page_size * page_count)
    }

    /// 删除请求/响应正文与 trace 附件;调用汇总与 trace 记录保留。
    pub(crate) async fn clear_detail_storage(&self) -> Result<()> {
        let _write = self.writes.lock().await;
        let mut transaction = self.pool.begin().await?;
        Self::clear_detail_storage_tx(&mut transaction).await?;
        transaction.commit().await?;
        Ok(())
    }

    async fn clear_detail_storage_tx(
        transaction: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    ) -> Result<()> {
        sqlx::query("DELETE FROM llm_call_requests")
            .execute(&mut **transaction)
            .await?;
        sqlx::query("DELETE FROM llm_call_response_chunks")
            .execute(&mut **transaction)
            .await?;
        // 正文清空后，调用记录不能再声称有详细记录，trace 也不能保留已不存在的字节数。
        sqlx::query("UPDATE llm_calls SET detailed = 0 WHERE detailed = 1")
            .execute(&mut **transaction)
            .await?;
        sqlx::query(
            "UPDATE cursor_run_traces SET request_bytes = 0, response_bytes = 0,
             response_event_count = 0",
        )
        .execute(&mut **transaction)
        .await?;
        Self::clear_trace_artifacts_tx(transaction).await
    }

    async fn clear_trace_artifacts_tx(
        transaction: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    ) -> Result<()> {
        sqlx::query(
            "CREATE TEMP TABLE IF NOT EXISTS clear_statistics_blob_ids(
                blob_id BLOB PRIMARY KEY
             )",
        )
        .execute(&mut **transaction)
        .await?;
        sqlx::query("DELETE FROM clear_statistics_blob_ids")
            .execute(&mut **transaction)
            .await?;
        sqlx::query(
            "INSERT OR IGNORE INTO clear_statistics_blob_ids(blob_id)
             SELECT blob_id FROM cursor_run_trace_artifacts",
        )
        .execute(&mut **transaction)
        .await?;
        sqlx::query("DELETE FROM cursor_run_trace_artifacts")
            .execute(&mut **transaction)
            .await?;
        sqlx::query(
            "DELETE FROM blobs
             WHERE blob_id IN (SELECT blob_id FROM clear_statistics_blob_ids)
               AND NOT EXISTS (
                   SELECT 1 FROM cursor_run_trace_artifacts a WHERE a.blob_id = blobs.blob_id
               )
               AND NOT EXISTS (
                   SELECT 1 FROM blob_edges e
                   WHERE e.parent_blob_id = blobs.blob_id OR e.child_blob_id = blobs.blob_id
               )",
        )
        .execute(&mut **transaction)
        .await?;
        sqlx::query("DROP TABLE clear_statistics_blob_ids")
            .execute(&mut **transaction)
            .await?;
        Ok(())
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ModelConfigInput, ModelType, OPENAI_CHAT_ENDPOINT};

    #[tokio::test]
    async fn clears_detail_storage_without_removing_configuration() {
        let store = Store::connect("sqlite::memory:").await.unwrap();
        store
            .create_model(&ModelConfigInput {
                sort_order: 0,
                display_name: "Model".into(),
                group_name: None,
                model_type: ModelType::OpenAi,
                base_url: "https://example.com/v1/chat/completions".into(),
                use_full_url: true,
                api_key: "secret".into(),
                tooltip_data: "Model".into(),
                model_id: "model".into(),
                reasoning_effort: None,
                effort_options: crate::model::DEFAULT_EFFORT_OPTIONS
                    .iter()
                    .map(|value| (*value).into())
                    .collect(),
                context_options: crate::model::DEFAULT_CONTEXT_OPTIONS
                    .iter()
                    .map(|value| (*value).into())
                    .collect(),
                openai_endpoint: OPENAI_CHAT_ENDPOINT.into(),
                openai_extra_params_enabled: false,
                openai_extra_params: serde_json::json!({}),
                custom_headers_enabled: false,
                custom_headers: serde_json::json!({}),
                anthropic_extra_params_enabled: false,
                anthropic_extra_params: serde_json::json!({}),
                context_window_tokens: None,
                max_completion_tokens: None,
                anthropic_max_tokens: None,
                anthropic_thinking_effort: None,
                thinking_budget_tokens: None,
            })
            .await
            .unwrap();
        sqlx::query("INSERT INTO llm_calls(call_id, run_id, conversation_id, provider_call_index, provider_type, provider_url, request_type, request_url, model_id, display_name, status, created_at_ms, message_count, tool_count, detailed) VALUES ('call-1', 'run-1', 'conversation-1', 0, 'openai-chat', 'https://example.com', 'openai-chat', 'https://example.com/v1/chat/completions', 'model', 'Model', 'completed', 1, 1, 0, 0)")
            .execute(store.pool()).await.unwrap();

        store.clear_detail_storage().await.unwrap();
        assert!(
            store.llm_call("call-1").await.unwrap().is_some(),
            "调用汇总仍然保留"
        );
        assert!(
            store.database_bytes().await.unwrap() > 0,
            "数据库文件占用始终非零"
        );
        assert!(store.llm_call_request("call-1").await.unwrap().is_none());
        assert!(store.llm_call_chunks("call-1").await.unwrap().is_empty());
        let model_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM model_configs")
            .fetch_one(store.pool())
            .await
            .unwrap();

        assert_eq!(model_count, 1);

        store
            .record_llm_request(
                "call-1",
                &serde_json::json!({}),
                &serde_json::json!({"model": "model"}),
                true,
            )
            .await
            .unwrap();
        store
            .record_llm_chunk("call-1", 0, 1, b"data", true)
            .await
            .unwrap();

        assert!(store.llm_call_request("call-1").await.unwrap().is_some());
        assert_eq!(store.llm_call_chunks("call-1").await.unwrap().len(), 1);
        store.clear_detail_storage().await.unwrap();
        assert!(store.llm_call_request("call-1").await.unwrap().is_none());
        assert!(store.llm_call_chunks("call-1").await.unwrap().is_empty());
        assert!(store.llm_call("call-1").await.unwrap().is_some());
    }
}
