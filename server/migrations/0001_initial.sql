-- Final schema.
PRAGMA foreign_keys = ON;

-- 每个 Cursor 会话的身份与头部状态:head checkpoint、活跃 run、会话级模型选择
-- (ModelSelection JSON);首次请求随根 checkpoint 创建,空会话由维护清理。
CREATE TABLE IF NOT EXISTS conversations (
    conversation_id TEXT PRIMARY KEY,
    current_checkpoint_id INTEGER,
    active_run_id TEXT,
    model_selection TEXT,
    updated_at_ms INTEGER NOT NULL
);

-- 会话的 append-only 规范化消息日志,内容在 payload_json;由 checkpoint_messages
-- 按谱系引用;runtime_event_id 部分唯一索引保证运行时事件重投幂等。
CREATE TABLE IF NOT EXISTS messages (
    conversation_id TEXT NOT NULL,
    message_id TEXT NOT NULL,
    role TEXT NOT NULL CHECK(role IN ('system', 'user', 'assistant', 'tool')),
    origin TEXT NOT NULL CHECK(origin IN ('prompt', 'user', 'runtime', 'assistant', 'tool')),
    payload_json TEXT NOT NULL,
    runtime_event_id TEXT,
    created_at_ms INTEGER NOT NULL,
    PRIMARY KEY (conversation_id, message_id),
    FOREIGN KEY (conversation_id) REFERENCES conversations(conversation_id)
);

CREATE UNIQUE INDEX IF NOT EXISTS messages_runtime_event
ON messages(conversation_id, runtime_event_id)
WHERE runtime_event_id IS NOT NULL;

-- 不可变消息状态快照树:parent 指向父快照(根为 NULL),state_digest 为沿父链
-- 全部消息的 32 字节 SHA-256;UNIQUE(会话,digest) 保证同一状态只落库一次。
CREATE TABLE IF NOT EXISTS conversation_checkpoints (
    checkpoint_id INTEGER PRIMARY KEY AUTOINCREMENT,
    conversation_id TEXT NOT NULL,
    parent_checkpoint_id INTEGER,
    state_digest BLOB NOT NULL CHECK(length(state_digest) = 32),
    created_at_ms INTEGER NOT NULL,
    UNIQUE (conversation_id, state_digest),
    FOREIGN KEY (conversation_id) REFERENCES conversations(conversation_id),
    FOREIGN KEY (parent_checkpoint_id) REFERENCES conversation_checkpoints(checkpoint_id)
);

CREATE INDEX IF NOT EXISTS conversation_checkpoints_parent
ON conversation_checkpoints(conversation_id, parent_checkpoint_id);

-- 每个 checkpoint 相对父节点新增消息的有序序列(ordinal 从 0 递增);
-- 沿谱系按 depth 降序、ordinal 升序拼接即完整上下文。
CREATE TABLE IF NOT EXISTS checkpoint_messages (
    checkpoint_id INTEGER NOT NULL,
    ordinal INTEGER NOT NULL CHECK(ordinal >= 0),
    conversation_id TEXT NOT NULL,
    message_id TEXT NOT NULL,
    PRIMARY KEY (checkpoint_id, ordinal),
    UNIQUE (checkpoint_id, message_id),
    FOREIGN KEY (checkpoint_id) REFERENCES conversation_checkpoints(checkpoint_id),
    FOREIGN KEY (conversation_id, message_id) REFERENCES messages(conversation_id, message_id)
);

-- 每次代理执行的归属与生命周期台账:base/head checkpoint、状态机、provider 调用计数、
-- 失败摘要;用于会话活跃 run 互斥与所有权校验。
CREATE TABLE IF NOT EXISTS runs (
    run_id TEXT PRIMARY KEY,
    conversation_id TEXT NOT NULL,
    base_checkpoint_id INTEGER NOT NULL,
    head_checkpoint_id INTEGER NOT NULL,
    parent_run_id TEXT,
    parent_tool_call_id TEXT,
    run_kind TEXT NOT NULL CHECK(run_kind IN ('root', 'subagent')),
    subagent_kind TEXT,
    status TEXT NOT NULL CHECK(status IN ('running', 'completed', 'failed', 'cancelled')),
    cursor_request_id TEXT,
    provider_call_index INTEGER NOT NULL DEFAULT -1,
    failure_category TEXT,
    failure_summary TEXT,
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL,
    FOREIGN KEY (conversation_id) REFERENCES conversations(conversation_id),
    FOREIGN KEY (base_checkpoint_id) REFERENCES conversation_checkpoints(checkpoint_id),
    FOREIGN KEY (head_checkpoint_id) REFERENCES conversation_checkpoints(checkpoint_id)
);

CREATE INDEX IF NOT EXISTS runs_conversation_status
ON runs(conversation_id, status);

CREATE INDEX IF NOT EXISTS idx_runs_cursor_request_active
ON runs(cursor_request_id, status, created_at_ms DESC);

-- 工具调用轮次台账:assistant 消息与完成序号分配,写锁+BEGIN IMMEDIATE 下原子判定
-- 整轮 settled;base_checkpoint_id 兼作 GC 可达性锚点。
CREATE TABLE IF NOT EXISTS tool_rounds (
    round_id TEXT PRIMARY KEY,
    run_id TEXT NOT NULL,
    base_checkpoint_id INTEGER NOT NULL,
    assistant_json TEXT NOT NULL,
    status TEXT NOT NULL CHECK(status IN ('pending', 'settled')),
    version INTEGER NOT NULL DEFAULT 0,
    next_completion_seq INTEGER NOT NULL DEFAULT 0,
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL,
    FOREIGN KEY (run_id) REFERENCES runs(run_id),
    FOREIGN KEY (base_checkpoint_id) REFERENCES conversation_checkpoints(checkpoint_id)
);

CREATE INDEX IF NOT EXISTS tool_rounds_run_created
ON tool_rounds(run_id, created_at_ms DESC);

-- 轮次内每个工具调用的参数/解析错误/完成顺序(completion_seq,未完成 NULL)
-- 与提交 checkpoint;供重启恢复 pending 状态与 GC 锚定。
CREATE TABLE IF NOT EXISTS tool_round_calls (
    round_id TEXT NOT NULL,
    call_index INTEGER NOT NULL,
    call_id TEXT NOT NULL,
    model_call_id TEXT NOT NULL,
    name TEXT NOT NULL,
    arguments_json TEXT NOT NULL,
    argument_error TEXT,
    status TEXT NOT NULL CHECK(status IN ('pending', 'completed')),
    completion_seq INTEGER,
    committed_checkpoint_id INTEGER,
    PRIMARY KEY (round_id, call_index),
    UNIQUE (round_id, call_id),
    UNIQUE (round_id, completion_seq),
    FOREIGN KEY (round_id) REFERENCES tool_rounds(round_id),
    FOREIGN KEY (committed_checkpoint_id) REFERENCES conversation_checkpoints(checkpoint_id)
);

-- SHA-256 内容寻址二进制存储:checkpoint 树分片、请求上下文、工具结果图片、trace 附件正文。
CREATE TABLE IF NOT EXISTS blobs (
    blob_id BLOB PRIMARY KEY CHECK(length(blob_id) = 32),
    data BLOB NOT NULL,
    created_at_ms INTEGER NOT NULL
);

-- blob 父→子引用图(field_name 记录引用字段),供 GC 可达性闭包遍历与引用存在性检查。
CREATE TABLE IF NOT EXISTS blob_edges (
    parent_blob_id BLOB NOT NULL,
    child_blob_id BLOB NOT NULL,
    field_name TEXT NOT NULL,
    PRIMARY KEY (parent_blob_id, child_blob_id, field_name),
    FOREIGN KEY (parent_blob_id) REFERENCES blobs(blob_id),
    FOREIGN KEY (child_blob_id) REFERENCES blobs(blob_id)
);

CREATE INDEX IF NOT EXISTS blob_edges_child ON blob_edges(child_blob_id);

-- 输入锚点:按用户输入 input_id 去重,固定绑定首次计算的 base checkpoint,保证
-- 重投/重试复用同一基线,并为该 checkpoint 提供可达性根。
CREATE TABLE IF NOT EXISTS input_anchors (
    conversation_id TEXT NOT NULL,
    input_id TEXT NOT NULL,
    base_checkpoint_id INTEGER NOT NULL,
    created_at_ms INTEGER NOT NULL,
    PRIMARY KEY (conversation_id, input_id),
    FOREIGN KEY (conversation_id) REFERENCES conversations(conversation_id),
    FOREIGN KEY (base_checkpoint_id) REFERENCES conversation_checkpoints(checkpoint_id)
);

-- 用户自建 BYOK 模型配置(协议、地址、密钥、推理/上下文参数轴、展示元数据),
-- model_hash 主键,按 sort_order, display_name 排序供桌面端分组。
CREATE TABLE IF NOT EXISTS model_configs (
    model_hash TEXT PRIMARY KEY,
    sort_order INTEGER NOT NULL DEFAULT 0,
    display_name TEXT NOT NULL,
    group_name TEXT,
    model_type TEXT NOT NULL CHECK(model_type IN ('openai', 'anthropic')),
    base_url TEXT NOT NULL,
    use_full_url INTEGER NOT NULL DEFAULT 0 CHECK(use_full_url IN (0, 1)),
    api_key TEXT NOT NULL,
    tooltip_data TEXT NOT NULL,
    model_id TEXT NOT NULL,
    default_effort TEXT NOT NULL DEFAULT 'low',
    default_context TEXT NOT NULL DEFAULT '200k',
    effort_options_json TEXT NOT NULL DEFAULT '["low","medium","high","xhigh","max"]',
    context_options_json TEXT NOT NULL DEFAULT '["200k","356k","800k","1m"]',
    openai_endpoint TEXT NOT NULL DEFAULT '',
    openai_extra_params_enabled INTEGER NOT NULL DEFAULT 0 CHECK(openai_extra_params_enabled IN (0, 1)),
    openai_extra_params_json TEXT NOT NULL DEFAULT '{}',
    custom_headers_enabled INTEGER NOT NULL DEFAULT 0 CHECK(custom_headers_enabled IN (0, 1)),
    custom_headers_json TEXT NOT NULL DEFAULT '{}',
    anthropic_extra_params_enabled INTEGER NOT NULL DEFAULT 0 CHECK(anthropic_extra_params_enabled IN (0, 1)),
    anthropic_extra_params_json TEXT NOT NULL DEFAULT '{}',
    context_window_tokens INTEGER,
    max_completion_tokens INTEGER,
    anthropic_max_tokens INTEGER,
    thinking_budget_tokens INTEGER,
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS model_configs_sort ON model_configs(sort_order, display_name);

-- 服务级键值设置,每行一个 JSON 编码设置(详细日志、端口、出站代理、Commit、
-- 访问令牌、插件启停等);reset_database 保留本表。
CREATE TABLE IF NOT EXISTS service_settings (
    setting_key TEXT PRIMARY KEY NOT NULL,
    value_json TEXT NOT NULL,
    updated_at_ms INTEGER NOT NULL
);

INSERT OR IGNORE INTO service_settings(setting_key, value_json, updated_at_ms)
VALUES ('llm_detailed_logging', 'false', CAST(unixepoch('subsec') * 1000 AS INTEGER));

-- 每次上游模型调用的观测汇总:身份、终态、逐阶段时延、token/缓存用量;供调用记录页
-- 与用量聚合;作为历史数据保留,不随会话删除。
CREATE TABLE IF NOT EXISTS llm_calls (
    call_id TEXT PRIMARY KEY,
    run_id TEXT NOT NULL,
    conversation_id TEXT NOT NULL,
    provider_call_index INTEGER NOT NULL,
    model_hash TEXT,
    provider_type TEXT NOT NULL,
    provider_url TEXT NOT NULL,
    request_type TEXT NOT NULL CHECK(request_type IN ('openai-chat', 'openai-responses', 'anthropic', 'plugin')),
    request_url TEXT NOT NULL,
    model_id TEXT NOT NULL,
    display_name TEXT NOT NULL,
    status TEXT NOT NULL CHECK(status IN ('running', 'completed', 'error', 'cancelled')),
    finish_reason TEXT,
    created_at_ms INTEGER NOT NULL,
    request_started_at_ms INTEGER,
    response_headers_at_ms INTEGER,
    first_event_at_ms INTEGER,
    first_text_at_ms INTEGER,
    finished_at_ms INTEGER,
    ttfb_ms INTEGER,
    ttft_ms INTEGER,
    duration_ms INTEGER,
    input_tokens INTEGER,
    output_tokens INTEGER,
    total_tokens INTEGER,
    cache_read_tokens INTEGER,
    cache_write_tokens INTEGER,
    reasoning_tokens INTEGER,
    usage_json TEXT,
    message_count INTEGER NOT NULL,
    tool_count INTEGER NOT NULL,
    request_bytes INTEGER,
    response_bytes INTEGER NOT NULL DEFAULT 0,
    stream_event_count INTEGER NOT NULL DEFAULT 0,
    http_status INTEGER,
    error_kind TEXT,
    error_message TEXT,
    detailed INTEGER NOT NULL,
    reasoning_effort TEXT,
    fast INTEGER NOT NULL DEFAULT 0 CHECK (fast IN (0, 1)),
    first_valid_response_at_ms INTEGER,
    ttfr_ms INTEGER
);

CREATE INDEX IF NOT EXISTS llm_calls_created ON llm_calls(created_at_ms DESC);
CREATE INDEX IF NOT EXISTS llm_calls_run ON llm_calls(run_id, provider_call_index);
CREATE INDEX IF NOT EXISTS llm_calls_model ON llm_calls(model_hash, created_at_ms DESC);
CREATE INDEX IF NOT EXISTS llm_calls_conversation ON llm_calls(conversation_id, created_at_ms DESC);

-- 详细日志调用(llm_calls.detailed=1)每次尝试的原始请求头/体快照,重试/failover
-- 时覆盖为最后一次尝试。
CREATE TABLE IF NOT EXISTS llm_call_requests (
    call_id TEXT PRIMARY KEY,
    headers_json TEXT NOT NULL,
    body_json TEXT NOT NULL,
    byte_count INTEGER NOT NULL,
    FOREIGN KEY(call_id) REFERENCES llm_calls(call_id) ON DELETE CASCADE
);

-- 详细日志调用的原始响应字节分片,seq 每次尝试从 0 递增,received_offset_ms
-- 相对该次尝试起点。
CREATE TABLE IF NOT EXISTS llm_call_response_chunks (
    call_id TEXT NOT NULL,
    seq INTEGER NOT NULL,
    received_offset_ms INTEGER NOT NULL,
    data BLOB NOT NULL,
    byte_count INTEGER NOT NULL,
    PRIMARY KEY(call_id, seq),
    FOREIGN KEY(call_id) REFERENCES llm_calls(call_id) ON DELETE CASCADE
);

-- 每个 Cursor 请求(官方转发或本地 BYOK)一次运行的生命周期指标:路由、状态、HTTP 状态、
-- 首响应/结束时间、字节与分块计数;供调用页展示并关联 provider 调用;正文在
-- cursor_run_trace_artifacts。
CREATE TABLE IF NOT EXISTS cursor_run_traces (
    request_id TEXT PRIMARY KEY,
    conversation_id TEXT,
    route TEXT NOT NULL CHECK(route IN ('local_byok', 'cursor_official')),
    model_id TEXT,
    status TEXT NOT NULL CHECK(status IN ('running', 'completed', 'error')),
    request_bytes INTEGER NOT NULL DEFAULT 0,
    response_bytes INTEGER NOT NULL DEFAULT 0,
    response_event_count INTEGER NOT NULL DEFAULT 0,
    http_status INTEGER,
    received_at_ms INTEGER NOT NULL,
    first_response_at_ms INTEGER,
    finished_at_ms INTEGER,
    error_message TEXT
);

CREATE INDEX IF NOT EXISTS cursor_run_traces_received
ON cursor_run_traces(received_at_ms DESC);

-- trace 事件工件顺序索引:正文以内容寻址存于 blobs,本表记 seq/类型/来源/元数据;
-- 兼作 blob GC 锚点,随清除详细记录/重置删除。
CREATE TABLE IF NOT EXISTS cursor_run_trace_artifacts (
    request_id TEXT NOT NULL,
    seq INTEGER NOT NULL,
    artifact_type TEXT NOT NULL CHECK(artifact_type IN (
        'bidi_request', 'client_message', 'history_projection', 'server_message',
        'checkpoint', 'run_sse_chunk', 'blob_set', 'blob_get'
    )),
    source TEXT NOT NULL CHECK(source IN ('cursor_client', 'byok_server', 'cursor_official')),
    blob_id BLOB NOT NULL CHECK(length(blob_id) = 32),
    metadata_json TEXT NOT NULL DEFAULT '{}',
    created_at_ms INTEGER NOT NULL,
    PRIMARY KEY(request_id, seq),
    FOREIGN KEY(request_id) REFERENCES cursor_run_traces(request_id) ON DELETE CASCADE,
    FOREIGN KEY(blob_id) REFERENCES blobs(blob_id)
);

CREATE INDEX IF NOT EXISTS cursor_run_trace_artifacts_blob
ON cursor_run_trace_artifacts(blob_id);

-- 后台完成通知的消费台账:父代理通过 await/前台 Resume 同步拿到结果的完成项登记
-- 在此,后续 at-least-once 重复通知按身份抑制,不再触发 follow-up。
-- 写入方有两处:await/前台 Resume 拿到终态后由 checkpoint 成功发布回填;完成通知
-- 终结 pending shell 等待时分发登记。读取按键 {kind}:{task}:{tool_call} 精确匹配
-- 抑制重复通知;consumed_at_ms 仅用于排查。
CREATE TABLE IF NOT EXISTS background_consumed (
    conversation_id TEXT NOT NULL,
    kind TEXT NOT NULL CHECK(kind IN ('BACKGROUND_TASK_KIND_SHELL', 'BACKGROUND_TASK_KIND_SUBAGENT')),
    task_identity TEXT NOT NULL,
    tool_call_id TEXT NOT NULL,
    consumed_at_ms INTEGER NOT NULL,
    PRIMARY KEY (conversation_id, kind, task_identity, tool_call_id),
    FOREIGN KEY (conversation_id) REFERENCES conversations(conversation_id) ON DELETE CASCADE
);
