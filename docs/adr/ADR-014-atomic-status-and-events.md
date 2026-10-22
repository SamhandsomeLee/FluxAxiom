# ADR-014：状态变更与 durable event 同事务

- 状态：已采用
- 日期：2026-10-22
- 依据：P0 执行方案 v1.1 §4.2、§4.3、§4.6、§6、§8.2、A1.2；V0.3 §5.2、§8.1.1

## 背景

A1.1 已经把 Session、Run、Attempt 写入 SQLite，但没有 events 表，也没有 blob 目录。`observe` / `put_blob` 返回 `Unsupported`。A1.2 要让状态变化带上可补读的 durable fact，并且 Engine 经 `HostPort` 提交的观测在返回前已经落库。

权威数据是状态表，不是全量事件溯源。事件是同事务写入的审计与订阅数据。`RunSnapshot` 从权威表组装。重启先读状态表。

Blob 按 sha256 存在 Host 数据目录。SQLite 事务包不住文件写入。

## 决定

1. Run / Attempt 的状态变更与对应 durable event 在**同一个 SQLite 事务**里提交。不允许先 COMMIT 状态再另开事务写 event，也不允许反过来。
2. `sequence` 按单个 `run_id` 从 1 递增，在写入 event 的事务中分配。不要求跨 Run 的全局总序。表上使用 `UNIQUE(run_id, sequence)`。分配序号的写事务使用 `BEGIN IMMEDIATE`，先拿到 reserved lock 再读 `MAX(sequence)+1`，避免多进程 WAL 下两个 DEFERRED 事务读到同一最大值。若仍撞上唯一约束，该事务失败，返回 `Storage`。订阅者按 `(run_id, sequence)` 幂等，按 `after_sequence` 补读。COMMIT 之后才能把事实交给订阅者。
3. Engine 不写库，也不发 `run.*` / `attempt.*`。这两类事件只由 Host 在状态事务里写。Engine 只通过 `HostPort` 提交观测、blob 和预算。
4. `put_blob` 先把原文写到数据目录，再返回 `BlobRef`。`observe` 返回时，对应 event 已经 COMMIT。调用顺序是先 blob 后 observe。
5. Blob 是文件，不进入 SQLite 事务。内容地址为 `sha256:` 加小写 hex；相同字节只保留一份。事务回滚后允许磁盘上留下无人引用的孤儿 blob。部分写入用临时文件再 rename 避免。孤儿 blob 的回收（GC）不在 P0 范围，留下它们不是缺陷。
6. `observe` 必须带齐契约 event payload 里 Host 编造不了的字段（模型的 usage / `finish_reason` / `latency_ms`，工具的路径、字节数、`truncated`、耗时）。工具的 `byte_count` 是返回给模型的字节数，与 `result_ref` 的内容等长，不是截断前的原始长度。缺字段或字段为空时返回 `Invalid`，不写 event。原文仍只在 blob。
7. `HostPortError` 增加无载荷的 `Storage` 与 `Invalid`，并保持 `Copy`。`Unsupported` 表示本阶段没有这个能力；`Invalid` 表示观测未通过校验，尚未写入；`Storage` 表示能力已落地但这次持久化失败。写库或写 blob 失败不得装成不支持。SQLite / 路径等细节只写 Host 日志，不放进错误值，避免把路径带给 Engine。Engine 对 `Invalid` 和 `Storage` 一律收尾并交回失败，不解析错误文本。
8. A1.2 不实现预算记账、Ctrl-C、租约过期扫描和 CLI 组装。这些仍分别属于 A2、A3。

## 后果

A1.2 的后续提交先给 `EngineObservation` 补齐 payload 字段，并给 `HostPortError` 加上无载荷的 `Storage` 与 `Invalid`（保留 `Copy`），再在 `run-host` 建 `events` / `artifacts`、`UNIQUE(run_id, sequence)`、`BEGIN IMMEDIATE`、blob 目录，以及 `events_after` / `RunSnapshot`。回滚验收用测试钩子在 COMMIT 前失败，断言状态行和 event 都不在。不实现 blob GC。

本决定不把 blob 文件纳入 SQLite 原子性，也不要求用事件日志重建全部状态。租约、中断与重启语义仍是 ADR-021。

## 复审触发器

出现「状态已变但没有对应 event」或「event 已可见但状态未提交」；或者把文件写或外部副作用假装成与 SQLite 同行提交。
