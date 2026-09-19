CREATE TABLE consumed_background_completions (
    conversation_id TEXT NOT NULL,
    subagent_id TEXT NOT NULL,
    parent_tool_call_id TEXT NOT NULL,
    created_at_ms INTEGER NOT NULL,
    PRIMARY KEY (conversation_id, subagent_id, parent_tool_call_id),
    FOREIGN KEY (conversation_id) REFERENCES conversations(conversation_id) ON DELETE CASCADE
);
