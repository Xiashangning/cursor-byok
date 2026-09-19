-- develop 专属迁移(1000+ 号段,避让上游未来的 0011+):后台完成通知的消费台账。
-- 父代理通过 await/前台 Resume 同步拿到结果的完成项登记在此,后续 at-least-once
-- 重复通知按身份抑制,不再触发 follow-up。
CREATE TABLE background_consumed (
    conversation_id TEXT NOT NULL,
    kind TEXT NOT NULL,
    task_identity TEXT NOT NULL,
    tool_call_id TEXT NOT NULL,
    consumed_at_ms INTEGER NOT NULL,
    PRIMARY KEY (conversation_id, kind, task_identity, tool_call_id),
    FOREIGN KEY (conversation_id) REFERENCES conversations(conversation_id) ON DELETE CASCADE
);
