-- A1.2：durable event 与 Artifact 索引。blob 正文在数据目录，不进这些表。

CREATE TABLE events (
    event_id TEXT PRIMARY KEY NOT NULL,
    schema_version TEXT NOT NULL,
    run_id TEXT NOT NULL REFERENCES runs (run_id),
    -- 单个 Run 内从 1 递增。与 run_id 一起唯一，避免两个写事务拿到同一个序号。
    sequence INTEGER NOT NULL,
    -- run.* 用 run_id，attempt.* 和模型/工具调用用 attempt_id，artifact.created 用 artifact_id。
    subject_id TEXT NOT NULL,
    occurred_at TEXT NOT NULL,
    event_type TEXT NOT NULL,
    -- 契约 payload 的 JSON。不含文件全文、prompt 或 reasoning_content。
    payload TEXT NOT NULL,
    UNIQUE (run_id, sequence)
);

CREATE TABLE artifacts (
    artifact_id TEXT PRIMARY KEY NOT NULL,
    run_id TEXT NOT NULL REFERENCES runs (run_id),
    artifact_type TEXT NOT NULL,
    -- sha256 引用。正文在 blob 文件里。
    storage_ref TEXT NOT NULL,
    summary TEXT NOT NULL,
    created_at TEXT NOT NULL
);

CREATE INDEX artifacts_by_run ON artifacts (run_id, created_at);
