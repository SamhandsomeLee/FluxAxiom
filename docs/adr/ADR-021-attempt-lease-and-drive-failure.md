# ADR-021：Attempt 租约、中断语义与 drive 失败终态

- 状态：已采用（A3a 落地 §4.7.4；Ctrl-C / 孤儿扫描实现留给 A3b）
- 日期：2026-10-24
- 依据：P0 执行方案 v1.1 §4.2、§4.7.4、§4.7.6、§4.8；V0.3 §8.1；ADR-019

## 背景

A1 起 Attempt 就带 `owner` 与 `lease_expires_at`，但未续期、未扫描。A2.2 的 `prepare_drive` 在任务 blob 或工作区打开失败时直接返回错误，Run 停在 `Accepted`，无终态事件（§4.7.4 第 1 项）。预算已用量只在单次 `drive_run` 的内存里（§4.7.6），同 Run 再 drive 会重置计数（§4.7.4 第 3 项）。A3 开始前必须把这些缺口写成决定。

## 决定

### 1. `prepare_drive` 失败（§4.7.4 第 1 项）

采用方案 **(1)**：在仍为 `Accepted` / Attempt `Starting` 时，同一 `BEGIN IMMEDIATE` 事务写入：

- Attempt → `Ended`，`outcome = failed`，`error_reason` 为简短类别（如 `workspace root is unavailable`）
- Run → `Failed`，`terminal_reason = engine_error`
- `attempt.ended` 与 `run.failed` 各一条 durable event

不进入 `Running`，不调用 Engine。契约允许 `Accepted → Failed`；本 ADR 同时允许 `Starting → Ended`（从未真正跑过的 Attempt 直接收尾）。

`drive_run` 在完成上述终态后返回 `Ok(EngineOutcome::Failed { reason: EngineError, partial: None })`，便于 CLI 与成功路径一样读快照。存储/锁错误仍返回 `Err`。

### 2. `read_file` 大文件 I/O（§4.7.4 第 2 项）

A3a **不改**。触发条件（单文件数 MB、工具耗时明显偏大）未到之前保持执行方案原文与工具注释中的已知限制。触发后另开改动，不在本 ADR 预选游标方案。

### 3. 同 Run 再 drive 与预算（§4.7.4 第 3 项）

A3a / A3b **不允许**对同一 Run 再次 `drive_run`（仍仅 `Accepted` 可进入 `Running`；失败已终态化的 Run 不可重启）。因此本阶段不入库已用量，也不把续跑定义为新预算周期。

若以后允许续跑 / 重试同一 Run，必须先另写 ADR，在 (1) 已用量入库 与 (2) 新预算周期告知用户 之间二选一。

### 4. 中断与租约（§4.8；实现时机）

语义仍以执行方案 §4.8 为准：

- Ctrl-C → `Cancelling` → 取消令牌 → `Cancelled`
- `kill -9` / 崩溃 → 启动时扫描租约已过期的 `Running` / `Cancelling` Attempt → `Orphaned` + Run `Failed(host_restart)`

**A3a 不实现** Ctrl-C 接线、租约续期与启动扫描。**A3b** 实现它们。A3a 的 CLI 在 `run` 同步等待期间打印已提交事件；进程被强杀时 Run 可能停在 `Running`，直到 A3b 扫描收尾。

### 5. CLI 数据目录

- 环境变量 `FLUXAXIOM_DATA_DIR` 优先
- 否则：Windows 为 `%LOCALAPPDATA%\FluxAxiom`；其它平台为 `$XDG_DATA_HOME/fluxaxiom`（未设 XDG 时为 `~/.local/share/fluxaxiom`）
- `flux run --data-dir` 可覆盖单次调用

Host 数据与被分析的 `--workspace` 分离；不把库写进用户仓库。

## 后果

- 创建后工作区被删再 drive：用户看到 `Failed(engine_error)`，可用新 `request_id` 重试；旧 `request_id` 幂等仍返回该失败 Run。
- A3a 验收不声称 Ctrl-C / `host_restart`。
- ADR-022「创建之后目录消失停在 Accepted」的缺口由本决定关闭。

## 复审触发器

- 允许同 Run 续跑或显式 retry 命令
- 实现 A3b 时若发现租约/取消与本节冲突
- 需要把 prepare 失败从 `engine_error` 拆成更细的 `terminal_reason`
