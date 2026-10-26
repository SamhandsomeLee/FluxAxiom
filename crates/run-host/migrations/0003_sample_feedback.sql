-- A4b：人工反馈。只追加，不写 durable event，不改 last_sequence。

CREATE TABLE sample_feedback (
    feedback_id TEXT PRIMARY KEY NOT NULL,
    run_id TEXT NOT NULL REFERENCES runs (run_id),
    created_at TEXT NOT NULL,
    -- usable | needs_edit | redo
    verdict TEXT NOT NULL,
    -- 可选备注，应用层限 2000 字符；禁止粘贴密钥（文档提醒）。
    note TEXT
);

CREATE INDEX sample_feedback_by_run ON sample_feedback (run_id, created_at);
