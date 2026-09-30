//! Reconciles persisted state with what the running process can actually serve.
//!
//! Two jobs, both idempotent and both run once per process start:
//! - `recover_interrupted_state` closes out work the previous process left
//!   "running" and re-aligns the detail-logging flags with the payloads that
//!   survived a "clear details" request.
//! - `prune_orphaned_history` deletes checkpoints, checkpoint messages,
//!   messages, and Conversations that no read path can reach.
use serde::Serialize;
use sqlx::{Connection, SqliteConnection, SqlitePool};

use crate::{Error, Result};

use super::{now_ms, BlobId, Store};

/// 引用 Checkpoint / Message / Conversation 的下游表。
const CHECKPOINT_CHILDREN: &[&str] = &[
    "checkpoint_messages",
    "conversation_checkpoints",
    "conversations",
    "messages",
    "runs",
    "tool_rounds",
    "tool_round_calls",
    "input_anchors",
    "background_consumed",
];

/// 引用 Blob 的下游表。
const BLOB_CHILDREN: &[&str] = &["blob_edges", "cursor_run_trace_artifacts"];

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RecoveryReport {
    pub runs: u64,
    pub llm_calls: u64,
    pub traces: u64,
    pub detail_flags: u64,
    pub trace_counters: u64,
}

impl RecoveryReport {
    pub fn is_empty(self) -> bool {
        self == Self::default()
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PruneReport {
    pub checkpoints: u64,
    pub checkpoint_messages: u64,
    pub messages: u64,
    pub conversations: u64,
}

impl PruneReport {
    pub fn is_empty(self) -> bool {
        self == Self::default()
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BlobGcReport {
    pub blobs: u64,
    pub edges: u64,
}

impl BlobGcReport {
    pub fn is_empty(self) -> bool {
        self == Self::default()
    }
}

/// 数据库侧手动清理的结果。
#[derive(Clone, Copy, Debug, Default, Serialize)]
pub struct DatabaseCleanup {
    /// 清理后的数据库文件占用。
    pub bytes: i64,
    /// 数据库文件减少的字节数。
    pub freed_bytes: i64,
}

impl Store {
    /// 数据库侧的手动清理：删除请求/响应正文与 trace 附件、不可达历史、
    /// 无服务端引用的 blob 缓存，最后回收空闲页。调用汇总与配置保留。
    /// 文件缓存由控制层的 `clean_storage` 一并清除。
    pub async fn clean_database(&self) -> Result<DatabaseCleanup> {
        let before = self.database_bytes().await?;
        self.clear_detail_storage().await?;
        let prune = self.prune_orphaned_history().await?;
        let blobs = self.prune_unanchored_blobs().await?;
        {
            let _write = self.writes.lock().await;
            sqlx::query("VACUUM").execute(&self.pool).await?;
        }
        let bytes = self.database_bytes().await?;
        tracing::info!(
            ?prune,
            ?blobs,
            freed_bytes = before - bytes,
            "storage cleanup completed"
        );
        Ok(DatabaseCleanup {
            bytes,
            freed_bytes: (before - bytes).max(0),
        })
    }

    /// 批量删除会逐行触发外键校验,而子表没有支撑索引;维护期改用独立连接关闭校验,
    /// 结束前用一次 `foreign_key_check` 验证。连接用完即关闭,不会把状态留回连接池。
    async fn maintenance_connection(&self) -> Result<SqliteConnection> {
        let options = self.options.as_ref().clone().foreign_keys(false);
        Ok(SqliteConnection::connect_with(&options).await?)
    }

    /// 只校验维护会删除的父表的下游表:全库校验要扫 170 万行 blob_edges,代价过高。
    async fn verify_foreign_keys(pool: &SqlitePool, children: &[&str]) -> Result<()> {
        for table in children {
            let violations: i64 = sqlx::query_scalar(&format!(
                "SELECT COUNT(*) FROM pragma_foreign_key_check('{table}')"
            ))
            .fetch_one(pool)
            .await?;
            if violations > 0 {
                return Err(Error::Store(format!(
                    "database maintenance left {violations} foreign key violations in {table}"
                )));
            }
        }
        Ok(())
    }

    pub async fn recover_interrupted_state(&self) -> Result<RecoveryReport> {
        let _write = self.writes.lock().await;
        let now = now_ms();
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let runs = sqlx::query(
            "UPDATE runs SET status = 'failed', failure_category = 'interrupted',
             failure_summary = 'server restarted while the run was active', updated_at_ms = ?
             WHERE status = 'running'",
        )
        .bind(now)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        let llm_calls = sqlx::query(
            "UPDATE llm_calls SET status = 'error', error_kind = 'interrupted',
             error_message = 'server restarted while the call was active',
             finished_at_ms = ?, duration_ms = MAX(0, ? - created_at_ms)
             WHERE status = 'running'",
        )
        .bind(now)
        .bind(now)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        let traces = sqlx::query(
            "UPDATE cursor_run_traces SET status = 'error', finished_at_ms = ?,
             error_message = 'server restarted while the trace was active'
             WHERE status = 'running'",
        )
        .bind(now)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        sqlx::query(
            "UPDATE conversations SET active_run_id = NULL, updated_at_ms = ?
             WHERE active_run_id IS NOT NULL",
        )
        .bind(now)
        .execute(&mut *tx)
        .await?;
        // 详情开关和字节计数只有和实际留下的正文一致才有意义。
        let detail_flags = sqlx::query(
            "UPDATE llm_calls SET detailed = 0 WHERE detailed = 1
               AND NOT EXISTS (
                   SELECT 1 FROM llm_call_requests r WHERE r.call_id = llm_calls.call_id
               )
               AND NOT EXISTS (
                   SELECT 1 FROM llm_call_response_chunks c WHERE c.call_id = llm_calls.call_id
               )",
        )
        .execute(&mut *tx)
        .await?
        .rows_affected();
        let trace_counters = sqlx::query(
            "UPDATE cursor_run_traces SET request_bytes = 0, response_bytes = 0,
             response_event_count = 0
             WHERE (request_bytes <> 0 OR response_bytes <> 0 OR response_event_count <> 0)
               AND NOT EXISTS (
                   SELECT 1 FROM cursor_run_trace_artifacts a
                   WHERE a.request_id = cursor_run_traces.request_id
               )",
        )
        .execute(&mut *tx)
        .await?
        .rows_affected();
        tx.commit().await?;
        Ok(RecoveryReport {
            runs,
            llm_calls,
            traces,
            detail_flags,
            trace_counters,
        })
    }

    /// 可达性锚点只有：会话当前 checkpoint、Run 的 base/head、Tool round 的
    /// base 与 committed checkpoint、输入锚点的 base。其余 checkpoint 无法被任何读路径加载。
    pub async fn prune_orphaned_history(&self) -> Result<PruneReport> {
        let _write = self.writes.lock().await;
        let mut tx = self.maintenance_connection().await?;
        sqlx::query("BEGIN IMMEDIATE").execute(&mut tx).await?;
        // 快路径：不可达分支的入口是「没有可达子节点的 checkpoint」。没有这种入口、
        // 也没有空会话时，不必做可达性闭包。
        let candidates: i64 = sqlx::query_scalar(
            "SELECT (SELECT COUNT(*) FROM conversation_checkpoints c
                     WHERE c.checkpoint_id NOT IN (SELECT current_checkpoint_id FROM conversations
                                                   WHERE current_checkpoint_id IS NOT NULL)
                       AND c.checkpoint_id NOT IN (SELECT base_checkpoint_id FROM runs)
                       AND c.checkpoint_id NOT IN (SELECT head_checkpoint_id FROM runs)
                       AND c.checkpoint_id NOT IN (SELECT base_checkpoint_id FROM tool_rounds)
                       AND c.checkpoint_id NOT IN (SELECT committed_checkpoint_id FROM tool_round_calls
                                                   WHERE committed_checkpoint_id IS NOT NULL)
                       AND c.checkpoint_id NOT IN (SELECT base_checkpoint_id FROM input_anchors)
                       AND NOT EXISTS (SELECT 1 FROM conversation_checkpoints child
                                       WHERE child.conversation_id = c.conversation_id
                                         AND child.parent_checkpoint_id = c.checkpoint_id))
                  + (SELECT COUNT(*) FROM conversations c
                     WHERE NOT EXISTS (SELECT 1 FROM messages m WHERE m.conversation_id = c.conversation_id)
                       AND NOT EXISTS (SELECT 1 FROM runs r WHERE r.conversation_id = c.conversation_id)
                       AND NOT EXISTS (SELECT 1 FROM input_anchors a WHERE a.conversation_id = c.conversation_id)
                       AND NOT EXISTS (SELECT 1 FROM background_consumed b
                                       WHERE b.conversation_id = c.conversation_id))",
        )
        .fetch_one(&mut tx)
        .await?;
        if candidates == 0 {
            sqlx::query("ROLLBACK").execute(&mut tx).await?;
            return Ok(PruneReport::default());
        }
        sqlx::query(
            "CREATE TEMP TABLE IF NOT EXISTS prune_live_checkpoints(
                 checkpoint_id INTEGER PRIMARY KEY
             )",
        )
        .execute(&mut tx)
        .await?;
        sqlx::query("DELETE FROM prune_live_checkpoints")
            .execute(&mut tx)
            .await?;
        sqlx::query(
            "INSERT OR IGNORE INTO prune_live_checkpoints(checkpoint_id)
             WITH RECURSIVE anchors(checkpoint_id) AS (
                 SELECT current_checkpoint_id FROM conversations
                 WHERE current_checkpoint_id IS NOT NULL
                 UNION SELECT base_checkpoint_id FROM runs
                 UNION SELECT head_checkpoint_id FROM runs
                 UNION SELECT base_checkpoint_id FROM tool_rounds
                 UNION SELECT committed_checkpoint_id FROM tool_round_calls
                 WHERE committed_checkpoint_id IS NOT NULL
                 UNION SELECT base_checkpoint_id FROM input_anchors
             ),
             live(checkpoint_id) AS (
                 SELECT checkpoint_id FROM anchors
                 UNION
                 SELECT c.parent_checkpoint_id
                 FROM conversation_checkpoints c
                 JOIN live ON live.checkpoint_id = c.checkpoint_id
                 WHERE c.parent_checkpoint_id IS NOT NULL
             )
             SELECT checkpoint_id FROM live",
        )
        .execute(&mut tx)
        .await?;
        sqlx::query(
            "CREATE TEMP TABLE IF NOT EXISTS prune_live_messages(
                 conversation_id TEXT NOT NULL,
                 message_id TEXT NOT NULL,
                 PRIMARY KEY (conversation_id, message_id)
             ) WITHOUT ROWID",
        )
        .execute(&mut tx)
        .await?;
        sqlx::query("DELETE FROM prune_live_messages")
            .execute(&mut tx)
            .await?;
        sqlx::query(
            "INSERT OR IGNORE INTO prune_live_messages(conversation_id, message_id)
             SELECT DISTINCT cm.conversation_id, cm.message_id
             FROM checkpoint_messages cm
             JOIN prune_live_checkpoints lc ON lc.checkpoint_id = cm.checkpoint_id",
        )
        .execute(&mut tx)
        .await?;
        sqlx::query(
            "CREATE TEMP TABLE IF NOT EXISTS prune_dead_messages(
                 conversation_id TEXT NOT NULL,
                 message_id TEXT NOT NULL,
                 PRIMARY KEY (conversation_id, message_id)
             ) WITHOUT ROWID",
        )
        .execute(&mut tx)
        .await?;
        sqlx::query("DELETE FROM prune_dead_messages")
            .execute(&mut tx)
            .await?;
        sqlx::query(
            "INSERT OR IGNORE INTO prune_dead_messages(conversation_id, message_id)
             SELECT DISTINCT cm.conversation_id, cm.message_id
             FROM checkpoint_messages cm
             WHERE cm.checkpoint_id NOT IN (SELECT checkpoint_id FROM prune_live_checkpoints)",
        )
        .execute(&mut tx)
        .await?;
        sqlx::query(
            "CREATE TEMP TABLE IF NOT EXISTS prune_empty_conversations(
                 conversation_id TEXT PRIMARY KEY
             ) WITHOUT ROWID",
        )
        .execute(&mut tx)
        .await?;
        sqlx::query("DELETE FROM prune_empty_conversations")
            .execute(&mut tx)
            .await?;
        sqlx::query(
            "INSERT OR IGNORE INTO prune_empty_conversations(conversation_id)
             SELECT c.conversation_id FROM conversations c
             WHERE NOT EXISTS (SELECT 1 FROM messages m WHERE m.conversation_id = c.conversation_id)
               AND NOT EXISTS (SELECT 1 FROM runs r WHERE r.conversation_id = c.conversation_id)
               AND NOT EXISTS (
                   SELECT 1 FROM input_anchors a WHERE a.conversation_id = c.conversation_id
               )
               AND NOT EXISTS (
                   SELECT 1 FROM background_consumed b WHERE b.conversation_id = c.conversation_id
               )",
        )
        .execute(&mut tx)
        .await?;

        let checkpoint_messages = sqlx::query(
            "DELETE FROM checkpoint_messages
             WHERE checkpoint_id NOT IN (SELECT checkpoint_id FROM prune_live_checkpoints)",
        )
        .execute(&mut tx)
        .await?
        .rows_affected();
        let checkpoints = sqlx::query(
            "DELETE FROM conversation_checkpoints
             WHERE checkpoint_id NOT IN (SELECT checkpoint_id FROM prune_live_checkpoints)",
        )
        .execute(&mut tx)
        .await?
        .rows_affected();
        let messages = sqlx::query(
            "DELETE FROM messages
             WHERE EXISTS (
                 SELECT 1 FROM prune_dead_messages dm
                 WHERE dm.conversation_id = messages.conversation_id
                   AND dm.message_id = messages.message_id
             )
             AND NOT EXISTS (
                 SELECT 1 FROM prune_live_messages lm
                 WHERE lm.conversation_id = messages.conversation_id
                   AND lm.message_id = messages.message_id
             )",
        )
        .execute(&mut tx)
        .await?
        .rows_affected();
        let empty_checkpoints = sqlx::query(
            "DELETE FROM conversation_checkpoints
             WHERE conversation_id IN (SELECT conversation_id FROM prune_empty_conversations)",
        )
        .execute(&mut tx)
        .await?
        .rows_affected();
        let conversations = sqlx::query(
            "DELETE FROM conversations
             WHERE conversation_id IN (SELECT conversation_id FROM prune_empty_conversations)",
        )
        .execute(&mut tx)
        .await?
        .rows_affected();
        sqlx::query("DROP TABLE prune_empty_conversations")
            .execute(&mut tx)
            .await?;
        sqlx::query("DROP TABLE prune_dead_messages")
            .execute(&mut tx)
            .await?;
        sqlx::query("DROP TABLE prune_live_messages")
            .execute(&mut tx)
            .await?;
        sqlx::query("DROP TABLE prune_live_checkpoints")
            .execute(&mut tx)
            .await?;
        sqlx::query("COMMIT").execute(&mut tx).await?;
        Self::verify_foreign_keys(&self.pool, CHECKPOINT_CHILDREN).await?;
        Ok(PruneReport {
            checkpoints: checkpoints + empty_checkpoints,
            checkpoint_messages,
            messages,
            conversations,
        })
    }

    /// 删除没有任何服务端读取路径的 blob 缓存。
    ///
    /// Cursor 会话状态的 CAS 树只由客户端持有根 ID：服务端重启后不再是权威副本，
    /// 需要时由客户端在 KV GET 时重新提供，或由 canonical messages 重新生成。
    /// 保留 trace artifact 直接引用的 blob、消息里工具结果图片引用的 blob，
    /// 以及它们通过 blob_edges 可达的闭包；其余 blob 与边一起删除。
    pub async fn prune_unanchored_blobs(&self) -> Result<BlobGcReport> {
        let _write = self.writes.lock().await;
        let mut tx = self.maintenance_connection().await?;
        sqlx::query("BEGIN IMMEDIATE").execute(&mut tx).await?;
        sqlx::query(
            "CREATE TEMP TABLE IF NOT EXISTS gc_anchors(
                 blob_id BLOB PRIMARY KEY,
                 depth INTEGER NOT NULL
             )",
        )
        .execute(&mut tx)
        .await?;
        sqlx::query("DELETE FROM gc_anchors")
            .execute(&mut tx)
            .await?;
        sqlx::query(
            "INSERT OR IGNORE INTO gc_anchors(blob_id, depth)
             SELECT blob_id, 0 FROM cursor_run_trace_artifacts",
        )
        .execute(&mut tx)
        .await?;
        // 工具结果图片由 hydrate_tool_images 从 blob 读取，必须保留。
        let referenced_images: Vec<String> = sqlx::query_scalar(
            "SELECT json_extract(payload_json, '$.content.image.blob_id') FROM messages
             WHERE json_extract(payload_json, '$.content.image.blob_id') IS NOT NULL",
        )
        .fetch_all(&mut tx)
        .await?;
        for raw in referenced_images {
            // 单条畸形引用不应让整次清理失败;该图片无法锚定,跳过并记录。
            let Ok(id) = BlobId::from_base64(&raw) else {
                tracing::warn!(blob_id = %raw, "ignoring malformed message blob id");
                continue;
            };
            sqlx::query("INSERT OR IGNORE INTO gc_anchors(blob_id, depth) VALUES (?, 0)")
                .bind(id.as_bytes().as_slice())
                .execute(&mut tx)
                .await?;
        }
        let mut depth = 0_i64;
        loop {
            let added = sqlx::query(
                "INSERT OR IGNORE INTO gc_anchors(blob_id, depth)
                 SELECT e.child_blob_id, ? + 1 FROM blob_edges e
                 JOIN gc_anchors a ON a.blob_id = e.parent_blob_id
                 WHERE a.depth = ?",
            )
            .bind(depth)
            .bind(depth)
            .execute(&mut tx)
            .await?
            .rows_affected();
            if added == 0 {
                break;
            }
            depth += 1;
        }
        let edges = sqlx::query(
            "DELETE FROM blob_edges
             WHERE parent_blob_id NOT IN (SELECT blob_id FROM gc_anchors)
                OR child_blob_id NOT IN (SELECT blob_id FROM gc_anchors)",
        )
        .execute(&mut tx)
        .await?
        .rows_affected();
        let blobs =
            sqlx::query("DELETE FROM blobs WHERE blob_id NOT IN (SELECT blob_id FROM gc_anchors)")
                .execute(&mut tx)
                .await?
                .rows_affected();
        sqlx::query("DROP TABLE gc_anchors")
            .execute(&mut tx)
            .await?;
        sqlx::query("COMMIT").execute(&mut tx).await?;
        Self::verify_foreign_keys(&self.pool, BLOB_CHILDREN).await?;
        Ok(BlobGcReport { blobs, edges })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::BlobEdge;

    async fn test_store() -> Store {
        Store::connect("sqlite::memory:").await.unwrap()
    }

    /// 只有从锚点沿 parent→child 可达的 blob 与边留下。
    #[tokio::test]
    async fn blob_gc_keeps_server_referenced_blobs_only() {
        let store = test_store().await;
        let trace = store.put_blob(b"trace-body", &[]).await.unwrap();
        let child = store.put_blob(b"child", &[]).await.unwrap();
        let anchored_root = store
            .put_blob(
                b"anchored-root",
                &[BlobEdge {
                    child: child.clone(),
                    field_name: "turns[0]".into(),
                }],
            )
            .await
            .unwrap();
        let orphan_root = store.put_blob(b"orphan-root", &[]).await.unwrap();
        let orphan_child = store.put_blob(b"orphan-child", &[]).await.unwrap();
        sqlx::query("INSERT INTO cursor_run_traces(request_id, route, status, received_at_ms) VALUES ('req-1', 'local_byok', 'completed', 1)")
            .execute(store.pool())
            .await
            .unwrap();
        store
            .link_cursor_trace_artifact(
                "req-1",
                "checkpoint",
                "byok_server",
                &anchored_root,
                &serde_json::json!({}),
            )
            .await
            .unwrap();
        sqlx::query("INSERT INTO cursor_run_trace_artifacts(request_id, seq, artifact_type, source, blob_id, metadata_json, created_at_ms) VALUES ('req-1', 1, 'server_frame', 'byok_server', ?, '{}', 1)")
            .bind(trace.as_bytes().as_slice())
            .execute(store.pool())
            .await
            .unwrap();
        // 需要保留：消息里工具结果图片引用的 blob。
        let image = store.put_blob(b"image", &[]).await.unwrap();
        sqlx::query(
            "INSERT INTO conversations(conversation_id, updated_at_ms) VALUES ('conversation', 1)",
        )
        .execute(store.pool())
        .await
        .unwrap();
        let image_payload = serde_json::json!({
            "content": {"image": {"blob_id": image.to_base64(), "mime_type": "image/png", "path": "/tmp/a.png"}}
        });
        sqlx::query("INSERT INTO messages(conversation_id, message_id, role, origin, payload_json, created_at_ms) VALUES ('conversation', 'message-1', 'tool', 'tool', ?, 1)")
            .bind(image_payload.to_string())
            .execute(store.pool())
            .await
            .unwrap();

        let report = store.prune_unanchored_blobs().await.unwrap();

        assert_eq!(report.blobs, 2);
        assert_eq!(report.edges, 0);
        assert!(store.get_blob(&trace).await.unwrap().is_some());
        assert!(store.get_blob(&child).await.unwrap().is_some());
        assert!(store.get_blob(&anchored_root).await.unwrap().is_some());
        assert!(store.get_blob(&image).await.unwrap().is_some());
        assert!(store.get_blob(&orphan_root).await.unwrap().is_none());
        assert!(store.get_blob(&orphan_child).await.unwrap().is_none());
        assert!(store.prune_unanchored_blobs().await.unwrap().is_empty());
    }

    /// 无法被任何读路径加载的 checkpoint 与其独占的 message 一起删除。
    #[tokio::test]
    async fn prune_removes_unreachable_checkpoints_and_messages() {
        let store = test_store().await;
        sqlx::query(
            "INSERT INTO conversations(conversation_id, updated_at_ms) VALUES ('conversation', 1)",
        )
        .execute(store.pool())
        .await
        .unwrap();
        for (checkpoint_id, parent, digest) in
            [(1_i64, None, 1_u8), (2, Some(1), 2), (3, Some(2), 3)]
        {
            sqlx::query("INSERT INTO conversation_checkpoints(checkpoint_id, conversation_id, parent_checkpoint_id, state_digest, created_at_ms) VALUES (?, 'conversation', ?, ?, 1)")
                .bind(checkpoint_id)
                .bind(parent)
                .bind(vec![digest; 32])
                .execute(store.pool())
                .await
                .unwrap();
        }
        sqlx::query("UPDATE conversations SET current_checkpoint_id = 1 WHERE conversation_id = 'conversation'")
            .execute(store.pool())
            .await
            .unwrap();
        for (message_id, origin) in [("message-1", "prompt"), ("message-2", "runtime")] {
            sqlx::query("INSERT INTO messages(conversation_id, message_id, role, origin, payload_json, created_at_ms) VALUES ('conversation', ?, 'user', ?, '{}', 1)")
                .bind(message_id)
                .bind(origin)
                .execute(store.pool())
                .await
                .unwrap();
        }
        for (checkpoint_id, ordinal, message_id) in [
            (1_i64, 0_i64, "message-1"),
            (2, 0, "message-1"),
            (3, 0, "message-1"),
            (3, 1, "message-2"),
        ] {
            sqlx::query("INSERT INTO checkpoint_messages(checkpoint_id, ordinal, conversation_id, message_id) VALUES (?, ?, 'conversation', ?)")
                .bind(checkpoint_id)
                .bind(ordinal)
                .bind(message_id)
                .execute(store.pool())
                .await
                .unwrap();
        }
        sqlx::query("INSERT INTO runs(run_id, conversation_id, base_checkpoint_id, head_checkpoint_id, run_kind, status, created_at_ms, updated_at_ms) VALUES ('run-1', 'conversation', 1, 2, 'root', 'completed', 1, 1)")
            .execute(store.pool())
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO conversations(conversation_id, updated_at_ms) VALUES ('empty', 1)",
        )
        .execute(store.pool())
        .await
        .unwrap();

        let report = store.prune_orphaned_history().await.unwrap();

        assert_eq!(report.checkpoints, 1, "checkpoint 3 only");
        assert_eq!(report.checkpoint_messages, 2);
        assert_eq!(report.messages, 1);
        assert_eq!(report.conversations, 1, "the empty conversation");
        let conversations: Vec<String> = sqlx::query_scalar(
            "SELECT conversation_id FROM conversations ORDER BY conversation_id",
        )
        .fetch_all(store.pool())
        .await
        .unwrap();
        assert_eq!(conversations, vec!["conversation"]);
        let remaining: Vec<i64> = sqlx::query_scalar(
            "SELECT checkpoint_id FROM conversation_checkpoints ORDER BY checkpoint_id",
        )
        .fetch_all(store.pool())
        .await
        .unwrap();
        assert_eq!(remaining, vec![1, 2]);
        let remaining: Vec<String> =
            sqlx::query_scalar("SELECT message_id FROM messages ORDER BY message_id")
                .fetch_all(store.pool())
                .await
                .unwrap();
        assert_eq!(remaining, vec!["message-1"]);
        assert!(store.prune_orphaned_history().await.unwrap().is_empty());
    }

    /// 上次进程留下的进行中状态收敛到终态，详情标记与正文对齐。
    #[tokio::test]
    async fn recovery_closes_interrupted_work_and_realigns_detail_flags() {
        let store = test_store().await;
        sqlx::query("INSERT INTO conversations(conversation_id, active_run_id, updated_at_ms) VALUES ('conversation', 'run-1', 1)")
            .execute(store.pool())
            .await
            .unwrap();
        sqlx::query("INSERT INTO conversation_checkpoints(checkpoint_id, conversation_id, parent_checkpoint_id, state_digest, created_at_ms) VALUES (1, 'conversation', NULL, ?, 1)")
            .bind(vec![0_u8; 32])
            .execute(store.pool())
            .await
            .unwrap();
        sqlx::query("INSERT INTO runs(run_id, conversation_id, base_checkpoint_id, head_checkpoint_id, run_kind, status, created_at_ms, updated_at_ms) VALUES ('run-1', 'conversation', 1, 1, 'root', 'running', 1, 1)")
            .execute(store.pool())
            .await
            .unwrap();
        sqlx::query("INSERT INTO llm_calls(call_id, run_id, conversation_id, provider_call_index, provider_type, provider_url, request_type, request_url, model_id, display_name, status, created_at_ms, message_count, tool_count, detailed) VALUES ('call-1', 'run-1', 'conversation', 0, 'openai-chat', 'https://example.com', 'openai-chat', 'https://example.com', 'model', 'Model', 'running', 1, 1, 0, 1)")
            .execute(store.pool())
            .await
            .unwrap();
        sqlx::query("INSERT INTO cursor_run_traces(request_id, route, status, request_bytes, response_bytes, response_event_count, received_at_ms) VALUES ('req-1', 'local_byok', 'running', 10, 20, 2, 1)")
            .execute(store.pool())
            .await
            .unwrap();

        let report = store.recover_interrupted_state().await.unwrap();

        assert_eq!(report.runs, 1);
        assert_eq!(report.llm_calls, 1);
        assert_eq!(report.traces, 1);
        assert_eq!(report.detail_flags, 1);
        assert_eq!(report.trace_counters, 1);
        let active: Option<String> = sqlx::query_scalar(
            "SELECT active_run_id FROM conversations WHERE conversation_id = 'conversation'",
        )
        .fetch_one(store.pool())
        .await
        .unwrap();
        assert_eq!(active, None);
        let (status, category): (String, Option<String>) =
            sqlx::query_as("SELECT status, failure_category FROM runs WHERE run_id = 'run-1'")
                .fetch_one(store.pool())
                .await
                .unwrap();
        assert_eq!(status, "failed");
        assert_eq!(category.as_deref(), Some("interrupted"));
        let (status, detailed): (String, bool) =
            sqlx::query_as("SELECT status, detailed FROM llm_calls WHERE call_id = 'call-1'")
                .fetch_one(store.pool())
                .await
                .unwrap();
        assert_eq!(status, "error");
        assert!(!detailed);
        let (request_bytes, response_bytes, events): (i64, i64, i64) = sqlx::query_as(
            "SELECT request_bytes, response_bytes, response_event_count FROM cursor_run_traces WHERE request_id = 'req-1'",
        )
        .fetch_one(store.pool())
        .await
        .unwrap();
        assert_eq!((request_bytes, response_bytes, events), (0, 0, 0));
        assert!(store.recover_interrupted_state().await.unwrap().is_empty());
    }

    /// 手动清理：删除不可达历史与无锚点缓存、回收空闲页，并报告释放的字节数。
    #[tokio::test]
    async fn clean_storage_prunes_and_reports_freed_bytes() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::connect(&format!(
            "sqlite://{}",
            directory.path().join("test.db").display()
        ))
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO conversations(conversation_id, updated_at_ms) VALUES ('conversation', 1)",
        )
        .execute(store.pool())
        .await
        .unwrap();
        sqlx::query("INSERT INTO conversation_checkpoints(checkpoint_id, conversation_id, parent_checkpoint_id, state_digest, created_at_ms) VALUES (1, 'conversation', NULL, ?, 1), (2, 'conversation', 1, ?, 2)")
            .bind(vec![1_u8; 32])
            .bind(vec![2_u8; 32])
            .execute(store.pool())
            .await
            .unwrap();
        sqlx::query("UPDATE conversations SET current_checkpoint_id = 1 WHERE conversation_id = 'conversation'")
            .execute(store.pool())
            .await
            .unwrap();
        sqlx::query("INSERT INTO messages(conversation_id, message_id, role, origin, payload_json, created_at_ms) VALUES ('conversation', 'message-1', 'user', 'user', '{}', 1)")
            .execute(store.pool())
            .await
            .unwrap();
        sqlx::query("INSERT INTO checkpoint_messages(checkpoint_id, ordinal, conversation_id, message_id) VALUES (1, 0, 'conversation', 'message-1'), (2, 0, 'conversation', 'message-1')")
            .execute(store.pool())
            .await
            .unwrap();
        let orphan = store.put_blob(&vec![7_u8; 200_000], &[]).await.unwrap();

        let maintenance = store.clean_database().await.unwrap();

        assert!(maintenance.freed_bytes >= 0);
        assert!(store.get_blob(&orphan).await.unwrap().is_none());
        let checkpoints: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM conversation_checkpoints")
            .fetch_one(store.pool())
            .await
            .unwrap();
        assert_eq!(checkpoints, 1, "unreachable checkpoint 2 is pruned");

        // 幂等：再清理一次不会有任何变化。
        let again = store.clean_database().await.unwrap();
        assert_eq!(again.freed_bytes, 0);
        assert_eq!(again.bytes, maintenance.bytes);
        assert!(store.get_blob(&orphan).await.unwrap().is_none());
    }
}
