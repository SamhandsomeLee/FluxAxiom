-- A1.1 只建状态表。events / blobs / effects 有了写入路径再加，避免空表冒充能力。

CREATE TABLE sessions (
    -- Host 拥有的会话主键。Engine 不读写这张表。
    session_id TEXT PRIMARY KEY NOT NULL,
    created_at TEXT NOT NULL
);

CREATE TABLE runs (
    run_id TEXT PRIMARY KEY NOT NULL,
    -- Host 受理幂等键，不是契约 Run 的字段。UNIQUE 保证重复请求落到同一行。
    request_id TEXT NOT NULL UNIQUE,
    session_id TEXT REFERENCES sessions (session_id),
    status TEXT NOT NULL,
    terminal_reason TEXT,
    -- A1.1 暂存任务文本。A1.2 改为 blob 引用后，此列不再放原文。
    input_ref TEXT NOT NULL,
    workspace_ref TEXT NOT NULL,
    max_model_calls INTEGER NOT NULL,
    max_tool_calls INTEGER NOT NULL,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

CREATE TABLE attempts (
    attempt_id TEXT PRIMARY KEY NOT NULL,
    run_id TEXT NOT NULL REFERENCES runs (run_id),
    engine_id TEXT NOT NULL,
    engine_version TEXT NOT NULL,
    -- 执行进程身份（PID 的十进制字符串），不是权限。A3 用它判断原进程是否还在。
    owner TEXT NOT NULL,
    -- 创建时写入。A1.1 不续期、不扫描过期、不标 Orphaned。
    lease_expires_at TEXT NOT NULL,
    status TEXT NOT NULL,
    outcome TEXT,
    error_reason TEXT
);

CREATE INDEX attempts_by_run ON attempts (run_id);
