CREATE TABLE background_completion_claims (
    conversation_id TEXT NOT NULL,
    task_kind TEXT NOT NULL CHECK (task_kind IN ('subagent', 'shell')),
    task_id TEXT NOT NULL,
    terminal_status TEXT NOT NULL CHECK (terminal_status IN ('success', 'error', 'aborted')),
    disposition TEXT NOT NULL CHECK (disposition IN ('consumed', 'projected')),
    payload_digest TEXT,
    runtime_event_id TEXT,
    created_at_ms INTEGER NOT NULL,
    PRIMARY KEY (conversation_id, task_kind, task_id),
    UNIQUE (conversation_id, runtime_event_id),
    CHECK (
        (disposition = 'consumed' AND payload_digest IS NULL AND runtime_event_id IS NULL)
        OR
        (disposition = 'projected' AND payload_digest IS NOT NULL AND runtime_event_id IS NOT NULL)
    ),
    FOREIGN KEY (conversation_id) REFERENCES conversations(conversation_id) ON DELETE CASCADE
);
