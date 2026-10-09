//! 后台完成通知的消费台账:父代理已通过 await/前台 Resume 同步拿到结果的
//! 完成项。读取方按投影事件身份(`{kind}:{task}:{tool_call}`)精确匹配。
use std::collections::HashSet;

use crate::{model::ConversationId, Result};

use super::{now_ms, Store};

impl Store {
    pub(crate) async fn record_consumed_background_completions(
        &self,
        conversation_id: &ConversationId,
        kind: &str,
        completions: &[(String, String)],
    ) -> Result<()> {
        if completions.is_empty() {
            return Ok(());
        }
        let _write = self.writes.lock().await;
        let mut transaction = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        // 台账写入可能先于该会话的首次 prepare(如通知匹配到 pending Await
        // 后不再建 Run):先在同事务内补齐 conversations 行,避免外键报错。
        Self::ensure_conversation_tx(&mut transaction, conversation_id).await?;
        for (task_identity, tool_call_id) in completions {
            sqlx::query(
                "INSERT OR IGNORE INTO background_consumed
                 (conversation_id, kind, task_identity, tool_call_id, consumed_at_ms)
                 VALUES (?, ?, ?, ?, ?)",
            )
            .bind(conversation_id.as_str())
            .bind(kind)
            .bind(task_identity)
            .bind(tool_call_id)
            .bind(now_ms())
            .execute(&mut *transaction)
            .await?;
        }
        transaction.commit().await?;
        Ok(())
    }

    /// 该会话全部已消费完成项的事件身份,与逐条投影的 `background-completed:`
    /// 事件 ID 中的身份段一致。
    pub(crate) async fn consumed_background_identities(
        &self,
        conversation_id: &ConversationId,
    ) -> Result<HashSet<String>> {
        let rows: Vec<(String, String, String)> = sqlx::query_as(
            "SELECT kind, task_identity, tool_call_id
             FROM background_consumed
             WHERE conversation_id = ?",
        )
        .bind(conversation_id.as_str())
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|(kind, task_identity, tool_call_id)| {
                format!("{kind}:{task_identity}:{tool_call_id}")
            })
            .collect())
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

        assert!(store
            .consumed_background_identities(&conversation_id)
            .await
            .unwrap()
            .is_empty());
        store
            .record_consumed_background_completions(
                &conversation_id,
                "BACKGROUND_TASK_KIND_SUBAGENT",
                &[("agent-1".into(), "task-call-1".into())],
            )
            .await
            .unwrap();
        // 主键幂等:重复登记不产生第二行。
        store
            .record_consumed_background_completions(
                &conversation_id,
                "BACKGROUND_TASK_KIND_SUBAGENT",
                &[("agent-1".into(), "task-call-1".into())],
            )
            .await
            .unwrap();
        let identities = store
            .consumed_background_identities(&conversation_id)
            .await
            .unwrap();
        assert_eq!(
            identities.iter().collect::<Vec<_>>(),
            ["BACKGROUND_TASK_KIND_SUBAGENT:agent-1:task-call-1"]
        );
    }

    /// 台账写入不依赖调用方先建会话:缺失的 conversations 行在同一事务内补齐,
    /// 外键约束下也不会报错(空键会话经统一解析后落到请求 id 上,同样适用)。
    #[tokio::test]
    async fn recording_creates_a_missing_conversation_row() {
        let directory = tempfile::tempdir().unwrap();
        let url = format!("sqlite://{}", directory.path().join("test.db").display());
        let store = Store::connect(&url).await.unwrap();
        let conversation_id = ConversationId::new("ledger-only-request");

        store
            .record_consumed_background_completions(
                &conversation_id,
                "BACKGROUND_TASK_KIND_SHELL",
                &[("42".into(), "shell-call".into())],
            )
            .await
            .unwrap();

        assert!(store.conversation_exists(&conversation_id).await.unwrap());
        assert_eq!(
            store
                .consumed_background_identities(&conversation_id)
                .await
                .unwrap(),
            HashSet::from(["BACKGROUND_TASK_KIND_SHELL:42:shell-call".to_owned()])
        );
    }
}
