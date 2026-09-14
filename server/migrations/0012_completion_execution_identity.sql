-- A reusable child/task ID is not an execution ID. Old rows retain an
-- unknown tool_call_id; they must never become a wildcard for future runs.
CREATE TABLE background_completion_executions (
    conversation_id TEXT NOT NULL,
    task_kind TEXT NOT NULL CHECK (task_kind IN ('subagent', 'shell')),
    task_id TEXT NOT NULL,
    tool_call_id TEXT CHECK (tool_call_id IS NULL OR length(tool_call_id) > 0),
    terminal_status TEXT NOT NULL CHECK (terminal_status IN ('success', 'error', 'aborted')),
    disposition TEXT NOT NULL CHECK (disposition IN ('consumed', 'projected')),
    payload_digest TEXT,
    runtime_event_id TEXT,
    created_at_ms INTEGER NOT NULL,
    processed INTEGER NOT NULL DEFAULT 0 CHECK (processed IN (0, 1)),
    handling_run_id TEXT,
    PRIMARY KEY (conversation_id, task_kind, task_id, tool_call_id),
    UNIQUE (conversation_id, runtime_event_id),
    CHECK (
        (disposition = 'consumed' AND payload_digest IS NULL AND runtime_event_id IS NULL)
        OR
        (disposition = 'projected' AND payload_digest IS NOT NULL AND runtime_event_id IS NOT NULL)
    ),
    FOREIGN KEY (conversation_id) REFERENCES conversations(conversation_id) ON DELETE CASCADE
);
INSERT INTO background_completion_executions (
    conversation_id, task_kind, task_id, tool_call_id, terminal_status,
    disposition, payload_digest, runtime_event_id, created_at_ms, processed
)
SELECT conversation_id, task_kind, task_id, NULL, terminal_status,
       disposition, payload_digest, runtime_event_id, created_at_ms, disposition = 'consumed'
FROM background_completion_claims;
DROP TABLE background_completion_claims;
ALTER TABLE background_completion_executions RENAME TO background_completion_claims;
